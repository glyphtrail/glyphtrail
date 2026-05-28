; Definitions
(function_declaration . (identifier) @name) @def.function
(class_declaration . (identifier) @name) @def.class
(object_declaration . (identifier) @name) @def.class

; Inheritance
(delegation_specifier (constructor_invocation (user_type (identifier) @extends)))
(delegation_specifier (user_type (identifier) @implements))

; Calls
(call_expression . (identifier) @call)

; Imports
(import (qualified_identifier) @import)

; Comments
[(line_comment) (block_comment)] @comment
