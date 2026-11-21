; Docs: comments directly above a definition, where EmmyLua's annotations go, and the `local` keyword in
; front of an assignment.
((comment)+ @doc . (_) @definition.doc)
((comment)+ @doc . (variable_declaration "local" @doc . (assignment_statement) @definition.doc))
(variable_declaration "local" @doc . (assignment_statement) @definition.doc)

; A function is named as it is written: `M.setup`, `Point:len`.
(function_declaration name: (_) @name body: (block)? @body) @definition.function
(assignment_statement
  (variable_list . name: (_) @name)
  (expression_list . value: (function_definition body: (block)? @body))) @definition.function

; Matching rules. A `;` between statements changes nothing, and "..." and '...' are the same string.
(empty_statement) @match.optional
"," @match.trailing
(string ["\"" "'"] @match.quote (#set! @match.quote alt "\" '"))
(comment) @match.collapse
