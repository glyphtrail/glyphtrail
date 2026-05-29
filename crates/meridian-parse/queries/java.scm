; Definitions
(method_declaration name: (identifier) @name) @def.method
(class_declaration name: (identifier) @name) @def.class
(interface_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum

; Inheritance
(superclass (type_identifier) @extends)
(super_interfaces (type_list (type_identifier) @implements))

; Calls
(method_invocation name: (identifier) @call)
; Constructor instantiation `new Bar()` references the class (#5).
(object_creation_expression type: (type_identifier) @call)

; Imports
(import_declaration (scoped_identifier) @import)

; Comments
[(line_comment) (block_comment)] @comment
