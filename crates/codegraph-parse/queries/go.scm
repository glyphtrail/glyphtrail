; Definitions
(function_declaration name: (identifier) @name) @def.function
(method_declaration name: (field_identifier) @name) @def.method
(type_declaration (type_spec name: (type_identifier) @name type: (struct_type))) @def.struct
(type_declaration (type_spec name: (type_identifier) @name type: (interface_type))) @def.interface

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (selector_expression field: (field_identifier) @call))

; Imports
(import_spec path: (interpreted_string_literal) @import)

; Comments
(comment) @comment
