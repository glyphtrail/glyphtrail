; Definitions
(function_definition name: (identifier) @name) @def.function
(class_definition name: (identifier) @name) @def.class

; Inheritance
(class_definition
  name: (identifier) @name
  superclasses: (argument_list (identifier) @extends)) @def.class

; Calls
(call function: (identifier) @call)
(call function: (attribute attribute: (identifier) @call))

; Imports
(import_statement name: (dotted_name) @import)
(import_from_statement module_name: (dotted_name) @import)
(aliased_import name: (dotted_name) @import)

; Comments
(comment) @comment
