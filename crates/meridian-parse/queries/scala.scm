; Definitions
(function_definition name: (identifier) @name) @def.function
(class_definition name: (identifier) @name) @def.class
(object_definition name: (identifier) @name) @def.module
(trait_definition name: (identifier) @name) @def.trait

; Calls
(call_expression function: (identifier) @call)

; Comments
(comment) @comment
