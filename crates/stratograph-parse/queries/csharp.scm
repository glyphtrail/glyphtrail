; Definitions
(class_declaration name: (identifier) @name) @def.class
(interface_declaration name: (identifier) @name) @def.interface
(struct_declaration name: (identifier) @name) @def.struct
(enum_declaration name: (identifier) @name) @def.enum
(record_declaration name: (identifier) @name) @def.class
(method_declaration name: (identifier) @name) @def.method

; Inheritance (C# lists the base class and interfaces together)
(base_list (identifier) @extends)

; Calls
(invocation_expression function: (identifier) @call)
(invocation_expression function: (member_access_expression name: (identifier) @call))

; Imports
(using_directive (qualified_name) @import)
(using_directive (identifier) @import)

; Comments
(comment) @comment
