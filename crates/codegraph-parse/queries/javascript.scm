; Definitions
(function_declaration name: (identifier) @name) @def.function
(method_definition name: (property_identifier) @name) @def.method
(class_declaration name: (identifier) @name) @def.class

; Inheritance
(class_declaration
  name: (identifier) @name
  (class_heritage (identifier) @extends)) @def.class

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))

; Imports
(import_statement source: (string) @import)
(call_expression
  function: (identifier) @_require
  arguments: (arguments (string) @import)
  (#eq? @_require "require"))

; Comments
(comment) @comment
