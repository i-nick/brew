# typed: strict
# frozen_string_literal: true

require_relative "../../../global"
require "sorbet/tapioca/utils"
require "debrew"

module Tapioca
  module Compilers
    class Attrables < Tapioca::Dsl::Compiler
      ATTRABLE_FILENAME = "attrable.rb"
      ConstantType = type_member { { fixed: Module } }

      sig { override.returns(T::Enumerable[Module]) }
      def self.gather_constants = Homebrew::Tapioca::Utils.named_objects_with_module(Attrable)

      sig { override.void }
      def decorate
        root.create_path(constant) do |klass|
          Homebrew::Tapioca::Utils.methods_from_file(constant, ATTRABLE_FILENAME)
                                  .each { |method| compile_attrable_method(klass, method) }
          Homebrew::Tapioca::Utils.methods_from_file(constant, ATTRABLE_FILENAME, class_methods: true)
                                  .each { |method| compile_attrable_method(klass, method, class_method: true) }
        end
      end

      private

      sig { params(klass: RBI::Scope, method: T.any(Method, UnboundMethod), class_method: T::Boolean).void }
      def compile_attrable_method(klass, method, class_method: false)
        raise "Unsupported arity for method #{method.name} - did `Attrable` change?" unless method.arity.zero?

        # attr_predicate
        klass.create_method(
          method.name.to_s,
          return_type:  "T::Boolean",
          class_method:,
        )
      end
    end
  end
end
