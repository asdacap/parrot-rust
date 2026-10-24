# Migrating prompt templates to MiniJinja

Scriban prompt templates are no longer supported. A configured `engine: scriban`
is rejected with the template's configuration path. Configuration loading does
not rewrite the user's file: migrate each affected template manually.

## Update the engine and syntax together

Set `engine: minijinja` and rewrite the template using native MiniJinja syntax.
For example:

```yaml
prompt_templates:
  custom.items:
    engine: minijinja
    template: |-
      {% for item in items %}{{ item }}
      {% endfor %}
    allowed_arguments: [items]
    required_arguments: [items]
```

MiniJinja uses `{{ expression }}` for values and `{% ... %}` for control flow.
A Scriban `{{ for item in items }}` / `{{ end }}` loop becomes
`{% for item in items %}` / `{% endfor %}`. Conditions use
`{% if condition %}` / `{% else %}` / `{% endif %}`. Review filters, functions,
whitespace, and rendered output rather than assuming the engines are compatible.

Keep argument declarations consistent with the caller: `required_arguments`
must be a subset of `allowed_arguments`. The engine change does not rename
arguments or convert the data passed by the caller.

Templates with no engine specified still use the scalar engine, with `{name}`
substitutions. Do not add `engine: minijinja` to a scalar template without also
updating its syntax. After editing, reload the configuration and check rendered
output for the argument values and branches the template uses.
