; Definitions
(function_declaration . (simple_identifier) @name) @def.function
(class_declaration (type_identifier) @name) @def.class
(protocol_declaration (type_identifier) @name) @def.interface

; Calls
(call_expression . (simple_identifier) @call)

; Comments
(comment) @comment
