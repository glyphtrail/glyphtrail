; Visual Basic .NET.

; Types. A `Module` is a static class.
(class_block name: (identifier) @name) @def.class
(structure_block name: (identifier) @name) @def.struct
(interface_block name: (identifier) @name) @def.interface
(enum_block name: (identifier) @name) @def.enum
(module_block name: (identifier) @name) @def.module

; Members.
(method_declaration name: (identifier) @name) @def.method
(property_declaration name: (identifier) @name) @def.method
(delegate_declaration name: (identifier) @name) @def.function
(const_declaration name: (identifier) @name) @def.constant

; Inheritance. The grammar (0.1.0) only accepts these clauses on the `Class`
; line, so the usual next-line `Inherits Base` is a parse error and yields no
; base edge.
(inherits_clause (type (namespace_name (identifier) @extends .)))
(implements_clause (type (namespace_name (identifier) @implements .)))

; Calls: `Foo(x)`, `obj.Foo(x)`, the parameterless statement `Foo`, `New Foo()`.
(invocation target: (identifier) @call)
(invocation target: (member_access member: (identifier) @call))
(call_statement (expression (identifier) @call))
(new_expression type: (type (namespace_name (identifier) @call .)))

; Imports: `Imports System.Collections.Generic`.
(imports_statement namespace: (namespace_name) @import)

; Comments.
(comment) @comment
