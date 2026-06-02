; Definitions
(function_declaration name: (identifier) @name) @def.function
; Arrow / function-expression bound to a name: `const handler = () => {}`,
; `const f = function () {}` — the dominant way functions and React/TSX
; components are declared in modern TS (#5).
(variable_declarator name: (identifier) @name value: (arrow_function)) @def.function
(variable_declarator name: (identifier) @name value: (function_expression)) @def.function
(method_definition name: (property_identifier) @name) @def.method
(class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum

; Inheritance
(extends_clause (identifier) @extends)
(implements_clause (type_identifier) @implements)

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
; Constructor instantiation `new Foo()` references the class (#5).
(new_expression constructor: (identifier) @call)

; Imports
(import_statement source: (string) @import)

; Comments
(comment) @comment
