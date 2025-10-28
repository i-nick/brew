# typed: strict
# frozen_string_literal: true

module OS
  module Linux
    module Cask
      module Installer
        extend T::Helpers

        requires_ancestor { ::Cask::Installer }

        sig { void }
        def check_stanza_os_requirements
          return unless artifacts.any? do |artifact|
            ::Cask::Artifact::MACOS_ONLY_ARTIFACTS.include?(artifact.class)
          end

          raise ::Cask::CaskError, "macOS is required for this software."
        end
      end
    end
  end
end

Cask::Installer.prepend(OS::Linux::Cask::Installer)
