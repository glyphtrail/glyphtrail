; Merlin 6502 assembly (#359). Maps the assembler's structure onto glyphtrail's
; code graph: line-starting labels are symbols, JSR/JMP to a label are calls,
; PUT/USE are includes, `*` banners and `;` trailers are comments.

; A label on a code (instruction) line is a callable routine.
(operation (label_def (global_label) @name)) @def.function
; A label on an assembler-directive line is a value symbol, not a routine: an
; equate (`=`) or a data definition (`ds`/`hex`/`dfb`/…) (#369). Finer
; equate-vs-data-vs-macro splitting can follow.
(pseudo_operation (label_def (global_label) @name)) @def.constant

; A JSR/JMP whose operand is a label calls that routine. A JSR to a raw address
; (`jsr $f3e2`) has no symbol, so it yields no edge.
(operation (op_jsr) (arg_jsr (addr (label_ref (global_label) @call))))
(operation (op_jmp) (arg_jmp (addr (label_ref (global_label) @call))))

; PUT / USE pull in another source file.
(arg_put (filename) @import)
(arg_use (filename) @import)

; Comments: trailing `;` on a line, and full-line `*` banners.
(comment) @comment
(heading) @comment
