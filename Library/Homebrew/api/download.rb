# typed: strict
# frozen_string_literal: true

require "downloadable"

module Homebrew
  module API
    class DownloadStrategy < CurlDownloadStrategy
      sig { override.returns(Pathname) }
      def symlink_location
        cache/name
      end
    end

    class Download
      include Downloadable

      sig {
        params(
          url:              String,
          checksum:         T.nilable(Checksum),
          mirrors:          T::Array[String],
          cache:            T.nilable(Pathname),
          require_checksum: T::Boolean,
        ).void
      }
      def initialize(url, checksum, mirrors: [], cache: nil, require_checksum: true)
        super()
        @url = T.let(URL.new(url, using: API::DownloadStrategy), URL)
        @checksum = checksum
        @mirrors = mirrors
        @cache = cache
        @require_checksum = require_checksum
      end

      sig { override.returns(API::DownloadStrategy) }
      def downloader
        T.cast(super, API::DownloadStrategy)
      end

      sig { override.returns(String) }
      def name
        download_name
      end

      sig { override.returns(String) }
      def download_type
        "API source"
      end

      sig { override.returns(Pathname) }
      def cache
        @cache || super
      end

      sig { returns(Pathname) }
      def symlink_location
        downloader.symlink_location
      end

      private

      sig { override.returns(T::Boolean) }
      def silence_checksum_missing_error?
        !@require_checksum
      end
    end
  end
end
