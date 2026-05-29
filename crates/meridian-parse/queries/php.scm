; Definitions
(function_definition name: (name) @name) @def.function
(method_declaration name: (name) @name) @def.method
(class_declaration name: (name) @name) @def.class
(interface_declaration name: (name) @name) @def.interface
(trait_declaration name: (name) @name) @def.trait
(enum_declaration name: (name) @name) @def.enum

; Calls
(function_call_expression function: (name) @call)
(member_call_expression name: (name) @call)
(scoped_call_expression name: (name) @call)

; Comments
(comment) @comment
