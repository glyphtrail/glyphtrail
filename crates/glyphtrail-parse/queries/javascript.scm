; Definitions
(function_declaration name: (identifier) @name) @def.function
; Arrow / function-expression bound to a name: `const handler = () => {}`,
; `const f = function () {}` — the dominant way functions and React components
; are declared in modern JS (#5).
(variable_declarator name: (identifier) @name value: (arrow_function)) @def.function
(variable_declarator name: (identifier) @name value: (function_expression)) @def.function
(method_definition name: (property_identifier) @name) @def.method
(class_declaration name: (identifier) @name) @def.class

; Inheritance
(class_declaration
  name: (identifier) @name
  (class_heritage (identifier) @extends)) @def.class

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
; Constructor instantiation `new Foo()` references the class (#5).
(new_expression constructor: (identifier) @call)

; Imports
(import_statement source: (string) @import)
(call_expression
  function: (identifier) @_require
  arguments: (arguments (string) @import)
  (#eq? @_require "require"))

; Comments
(comment) @comment
