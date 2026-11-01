; Docs: comments directly above a definition. Annotations are part of the declaration's modifiers.
([(line_comment) (block_comment)]+ @doc . (_) @definition.doc)

(class_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface
(enum_declaration name: (identifier) @name) @definition.enum
(record_declaration name: (identifier) @name) @definition.class
(annotation_type_declaration name: (identifier) @name) @definition.interface
(method_declaration name: (identifier) @name body: (block) @body) @definition.method
(method_declaration name: (identifier) @name !body) @definition.method
(constructor_declaration name: (identifier) @name body: (constructor_body) @body) @definition.method
(compact_constructor_declaration name: (identifier) @name body: (block) @body) @definition.method

; Matching rules.
"," @match.trailing
[(line_comment) (block_comment)] @match.collapse
