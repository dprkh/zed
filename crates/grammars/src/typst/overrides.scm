(string) @string

(math) @math

; Unclosed math blocks recover as errors; retain math pairing rules at their end.
((ERROR "$") @math.inclusive_with_whitespace
  (#match? @math.inclusive_with_whitespace "^\\$"))

[
  (raw_span)
  (raw_blck)
] @raw

(comment) @comment.inclusive
