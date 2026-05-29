; Definitions — Elixir is macro-driven: `def`/`defp`/`defmacro NAME(...)` is a
; `call` to the def macro whose first argument is the defined call, and
; `defmodule ALIAS do` names a module.
((call (identifier) @_kw (arguments (call (identifier) @name)))
 (#match? @_kw "^(def|defp|defmacro|defmacrop)$")) @def.function
((call (identifier) @_kw (arguments (alias) @name))
 (#eq? @_kw "defmodule")) @def.module

; Calls — a call whose target is a plain identifier, excluding the def macros
; and common directives so the def keyword itself isn't read as a call.
((call (identifier) @call)
 (#not-match? @call "^(def|defp|defmacro|defmacrop|defmodule|defstruct|defprotocol|defimpl|use|import|alias|require)$"))

; Comments
(comment) @comment
