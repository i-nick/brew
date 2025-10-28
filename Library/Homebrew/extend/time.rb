# typed: strong
# frozen_string_literal: true

require "time"
require "utils/output"

class Time
  include Utils::Output::Mixin

  # Backwards compatibility for formulae that used this ActiveSupport extension
  sig { returns(String) }
  def rfc3339
    odisabled "Time#rfc3339", "Time#xmlschema"
    xmlschema
  end
end
