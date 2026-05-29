; Definitions
(function_declaration name: (identifier) @name) @def.function
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
