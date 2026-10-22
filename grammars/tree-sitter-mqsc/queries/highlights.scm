; tree-sitter-mqsc highlight queries
;
; Colours for editors that support tree-sitter native highlighting
; (Neovim, Helix, Emacs, Zed, ...) and as the basis for the LSP server's
; semantic tokens.

; Comments
(comment) @comment

; Command verb (first word of a command)
(verb) @keyword

; Strings and numbers
(string) @string
(integer) @number

; Wildcards and ranges
(wildcard) @operator

; The word right after the verb is the object type (DISPLAY QLOCAL ...).
; Everything else is a plain identifier / parameter name; the LSP server
; refines these via semantic tokens once it resolves them against the model.
((command (verb) (word) @type)
  (#set! priority 15))

(word) @variable

; Structural punctuation
[
  "("
  ")"
  ","
  "="
] @punctuation.bracket
