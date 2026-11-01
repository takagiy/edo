; A section is named by its heading. Its own blocks are the body; child sections stay visible.
(section
  (atx_heading heading_content: (inline) @name)
  [(paragraph) (list) (fenced_code_block) (indented_code_block) (block_quote)
   (thematic_break) (html_block) (pipe_table) (link_reference_definition)] @body) @definition.section
(section (atx_heading heading_content: (inline) @name)) @definition.section

; Matching rules: paragraphs may be wrapped differently.
(inline) @match.collapse
