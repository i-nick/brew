# frozen_string_literal: true

RSpec.describe Kernel do
  let(:dir) { mktmpdir }

  describe "#interactive_shell" do
    let(:shell) { dir/"myshell" }

    it "starts an interactive shell session" do
      File.write shell, <<~SH
        #!/bin/sh
        echo called > "#{dir}/called"
      SH

      FileUtils.chmod 0755, shell

      ENV["SHELL"] = shell

      expect { interactive_shell }.not_to raise_error
      expect(dir/"called").to exist
    end
  end

  describe "#with_custom_locale" do
    it "temporarily overrides the system locale" do
      ENV["LC_ALL"] = "en_US.UTF-8"

      with_custom_locale("C") do
        expect(ENV.fetch("LC_ALL")).to eq("C")
      end

      expect(ENV.fetch("LC_ALL")).to eq("en_US.UTF-8")
    end
  end

  describe "#which" do
    let(:cmd) { dir/"foo" }

    before { FileUtils.touch cmd }

    it "returns the first executable that is found" do
      cmd.chmod 0744
      expect(which(File.basename(cmd), File.dirname(cmd))).to eq(cmd)
    end

    it "skips non-executables" do
      expect(which(File.basename(cmd), File.dirname(cmd))).to be_nil
    end

    it "skips malformed path and doesn't fail" do
      # 'which' should not fail if a path is malformed
      # see https://github.com/Homebrew/legacy-homebrew/issues/32789 for an example
      cmd.chmod 0744

      # ~~ will fail because ~foo resolves to foo's home and there is no '~' user
      path = ["~~", File.dirname(cmd)].join(File::PATH_SEPARATOR)
      expect(which(File.basename(cmd), path)).to eq(cmd)
    end
  end

  specify "#which_editor" do
    ENV["HOMEBREW_EDITOR"] = "vemate -w"
    ENV["HOMEBREW_PATH"] = dir

    editor = "#{dir}/vemate"
    FileUtils.touch editor
    FileUtils.chmod 0755, editor

    expect(which_editor).to eq("vemate -w")
  end

  specify "#disk_usage_readable" do
    expect(disk_usage_readable(1)).to eq("1B")
    expect(disk_usage_readable(999)).to eq("999B")
    expect(disk_usage_readable(1000)).to eq("1KB")
    expect(disk_usage_readable(1025)).to eq("1KB")
    expect(disk_usage_readable(4_404_020)).to eq("4.4MB")
    expect(disk_usage_readable(4_509_715_660)).to eq("4.5GB")
  end

  describe "#number_readable" do
    it "returns a string with thousands separators" do
      expect(number_readable(1)).to eq("1")
      expect(number_readable(1_000)).to eq("1,000")
      expect(number_readable(1_000_000)).to eq("1,000,000")
    end
  end

  specify "#truncate_text_to_approximate_size" do
    glue = "\n[...snip...]\n" # hard-coded copy from truncate_text_to_approximate_size
    n = 20
    long_s = "x" * 40

    s = truncate_text_to_approximate_size(long_s, n)
    expect(s.length).to eq(n)
    expect(s).to match(/^x+#{Regexp.escape(glue)}x+$/)

    s = truncate_text_to_approximate_size(long_s, n, front_weight: 0.0)
    expect(s).to eq(glue + ("x" * (n - glue.length)))

    s = truncate_text_to_approximate_size(long_s, n, front_weight: 1.0)
    expect(s).to eq(("x" * (n - glue.length)) + glue)
  end

  describe "#with_env" do
    it "sets environment variables within the block" do
      expect(ENV.fetch("PATH")).not_to eq("/bin")
      with_env(PATH: "/bin") do
        expect(ENV.fetch("PATH", nil)).to eq("/bin")
      end
    end

    it "restores ENV after the block" do
      with_env(PATH: "/bin") do
        expect(ENV.fetch("PATH", nil)).to eq("/bin")
      end
      path = ENV.fetch("PATH", nil)
      expect(path).not_to be_nil
      expect(path).not_to eq("/bin")
    end

    it "restores ENV if an exception is raised" do
      expect do
        with_env(PATH: "/bin") do
          raise StandardError, "boom"
        end
      end.to raise_error(StandardError)

      path = ENV.fetch("PATH", nil)
      expect(path).not_to be_nil
      expect(path).not_to eq("/bin")
    end
  end

  describe "#tap_and_name_comparison" do
    describe "both strings are only names" do
      it "alphabetizes the strings" do
        expect(%w[a b].sort(&tap_and_name_comparison)).to eq(%w[a b])
        expect(%w[b a].sort(&tap_and_name_comparison)).to eq(%w[a b])
      end
    end

    describe "both strings include tap" do
      it "alphabetizes the strings" do
        expect(%w[a/z/z b/z/z].sort(&tap_and_name_comparison)).to eq(%w[a/z/z b/z/z])
        expect(%w[b/z/z a/z/z].sort(&tap_and_name_comparison)).to eq(%w[a/z/z b/z/z])

        expect(%w[z/a/z z/b/z].sort(&tap_and_name_comparison)).to eq(%w[z/a/z z/b/z])
        expect(%w[z/b/z z/a/z].sort(&tap_and_name_comparison)).to eq(%w[z/a/z z/b/z])

        expect(%w[z/z/a z/z/b].sort(&tap_and_name_comparison)).to eq(%w[z/z/a z/z/b])
        expect(%w[z/z/b z/z/a].sort(&tap_and_name_comparison)).to eq(%w[z/z/a z/z/b])
      end
    end

    describe "only one string includes tap" do
      it "prefers the string without tap" do
        expect(%w[a/z/z z].sort(&tap_and_name_comparison)).to eq(%w[z a/z/z])
        expect(%w[z a/z/z].sort(&tap_and_name_comparison)).to eq(%w[z a/z/z])
      end
    end
  end
end
