# typed: strict
# frozen_string_literal: true

require "abstract_command"
require "services/system"
require "services/commands/list"
require "services/commands/cleanup"
require "services/commands/info"
require "services/commands/restart"
require "services/commands/run"
require "services/commands/start"
require "services/commands/stop"
require "services/commands/kill"

module Homebrew
  module Cmd
    class Services < AbstractCommand
      cmd_args do
        usage_banner <<~EOS
          `services` [<subcommand>]

          Manage background services with macOS' `launchctl`(1) daemon manager or
          Linux's `systemctl`(1) service manager.

          If `sudo` is passed, operate on `/Library/LaunchDaemons` or `/usr/lib/systemd/system`  (started at boot).
          Otherwise, operate on `~/Library/LaunchAgents` or `~/.config/systemd/user` (started at login).

          [`sudo`] `brew services` [`list`] (`--json`) (`--debug`):
          List information about all managed services for the current user (or root).
          Provides more output from Homebrew and `launchctl`(1) or `systemctl`(1) if run with `--debug`.

          [`sudo`] `brew services info` (<formula>|`--all`|`--json`):
          List all managed services for the current user (or root).

          [`sudo`] `brew services run` (<formula>|`--all`):
          Run the service <formula> without registering to launch at login (or boot).

          [`sudo`] `brew services start` (<formula>|`--all`|`--file=`):
          Start the service <formula> immediately and register it to launch at login (or boot).

          [`sudo`] `brew services stop` (<formula>|`--all`):
          Stop the service <formula> immediately and unregister it from launching at login (or boot).

          [`sudo`] `brew services kill` (<formula>|`--all`):
          Stop the service <formula> immediately but keep it registered to launch at login (or boot).

          [`sudo`] `brew services restart` (<formula>|`--all`):
          Stop (if necessary) and start the service <formula> immediately and register it to launch at login (or boot).

          [`sudo`] `brew services cleanup`:
          Remove all unused services.
        EOS
        flag "--file=", description: "Use the service file from this location to `start` the service."
        flag "--sudo-service-user=", description: "When run as root on macOS, run the service(s) as this user."
        flag "--max-wait=", description: "Wait at most this many seconds for `stop` to finish stopping a service. " \
                                         "Omit this flag or set this to zero (0) seconds to wait indefinitely."
        switch "--all", description: "Run <subcommand> on all services."
        switch "--json", description: "Output as JSON."
        switch "--no-wait", description: "Don't wait for `stop` to finish stopping the service."
        conflicts "--max-wait=", "--no-wait"
        named_args max: 2
      end

      sig { override.void }
      def run
        # pbpaste's exit status is a proxy for detecting the use of reattach-to-user-namespace
        if ENV["HOMEBREW_TMUX"] && (File.exist?("/usr/bin/pbpaste") && !quiet_system("/usr/bin/pbpaste"))
          raise UsageError,
                "`brew services` cannot run under tmux!"
        end

        # Keep this after the .parse to keep --help fast.
        require "utils"

        if !Homebrew::Services::System.launchctl? && !Homebrew::Services::System.systemctl?
          raise UsageError,
                "`brew services` is supported only on macOS or Linux (with systemd)!"
        end

        if (sudo_service_user = args.sudo_service_user)
          unless Homebrew::Services::System.root?
            raise UsageError,
                  "`brew services` is supported only when running as root!"
          end

          unless Homebrew::Services::System.launchctl?
            raise UsageError,
                  "`brew services --sudo-service-user` is currently supported only on macOS " \
                  "(but we'd love a PR to add Linux support)!"
          end

          Homebrew::Services::Cli.sudo_service_user = sudo_service_user
        end

        # Parse arguments.
        subcommand, formula, = args.named

        no_named_formula_commands = [
          *Homebrew::Services::Commands::List::TRIGGERS,
          *Homebrew::Services::Commands::Cleanup::TRIGGERS,
        ]
        if no_named_formula_commands.include?(subcommand)
          raise UsageError, "The `#{subcommand}` subcommand does not accept a formula argument!" if formula
          raise UsageError, "The `#{subcommand}` subcommand does not accept the --all argument!" if args.all?
        end

        if args.file
          if Homebrew::Services::Commands::Start::TRIGGERS.exclude?(subcommand)
            raise UsageError, "The `#{subcommand}` subcommand does not accept the --file= argument!"
          elsif args.all?
            raise UsageError, "The start subcommand does not accept the --all and --file= arguments at the same time!"
          end
        end

        opoo "The --all argument overrides provided formula argument!" if formula.present? && args.all?

        targets = if args.all?
          if subcommand == "start"
            Homebrew::Services::Formulae.available_services(
              loaded:    false,
              skip_root: !Homebrew::Services::System.root?,
            )
          elsif subcommand == "stop"
            Homebrew::Services::Formulae.available_services(
              loaded:    true,
              skip_root: !Homebrew::Services::System.root?,
            )
          else
            Homebrew::Services::Formulae.available_services
          end
        elsif formula
          [Homebrew::Services::FormulaWrapper.new(Formulary.factory(formula))]
        else
          []
        end

        # Exit successfully if --all was used but there is nothing to do
        return if args.all? && targets.empty?

        if Homebrew::Services::System.systemctl?
          ENV["DBUS_SESSION_BUS_ADDRESS"] = ENV.fetch("HOMEBREW_DBUS_SESSION_BUS_ADDRESS", nil)
          ENV["XDG_RUNTIME_DIR"] = ENV.fetch("HOMEBREW_XDG_RUNTIME_DIR", nil)
        end

        # Dispatch commands and aliases.
        case subcommand.presence
        when *Homebrew::Services::Commands::List::TRIGGERS
          Homebrew::Services::Commands::List.run(json: args.json?)
        when *Homebrew::Services::Commands::Cleanup::TRIGGERS
          Homebrew::Services::Commands::Cleanup.run
        when *Homebrew::Services::Commands::Info::TRIGGERS
          Homebrew::Services::Commands::Info.run(targets, verbose: args.verbose?, json: args.json?)
        when *Homebrew::Services::Commands::Restart::TRIGGERS
          Homebrew::Services::Commands::Restart.run(targets, verbose: args.verbose?)
        when *Homebrew::Services::Commands::Run::TRIGGERS
          Homebrew::Services::Commands::Run.run(targets, verbose: args.verbose?)
        when *Homebrew::Services::Commands::Start::TRIGGERS
          Homebrew::Services::Commands::Start.run(targets, args.file, verbose: args.verbose?)
        when *Homebrew::Services::Commands::Stop::TRIGGERS
          max_wait = args.max_wait.to_f
          Homebrew::Services::Commands::Stop.run(targets, verbose: args.verbose?, no_wait: args.no_wait?, max_wait:)
        when *Homebrew::Services::Commands::Kill::TRIGGERS
          Homebrew::Services::Commands::Kill.run(targets, verbose: args.verbose?)
        else
          raise UsageError, "unknown subcommand: `#{subcommand}`"
        end
      end
    end
  end
end
