; Docs: comments directly above a definition, decorators, and the docstring at the top of the body.
((comment)+ @doc . (_) @definition.doc)
(decorated_definition (decorator) @doc definition: (_) @definition.doc)

(function_definition
  name: (identifier) @name
  body: (block . (expression_statement (string) @doc)?) @body) @definition.function
(class_definition
  name: (identifier) @name
  body: (block . (expression_statement (string) @doc)?)) @definition.class

; Matching rules.
"," @match.trailing
(string_start) @match.quote (#set! @match.quote alt "\" '")
(string_end) @match.quote (#set! @match.quote alt "\" '")
(comment) @match.collapse
