---
name: New tool pattern
about: Add support for a new AI coding tool
title: 'tool: add <ToolName> pattern'
labels: feature
assignees: ''
---

## Tool name
e.g. Windsurf, Aider, Continue, Gemini Code Assist

## Session artifact locations
What files/directories does this tool create in a project?
```
.windsurf/
.windsurf/settings.json
```

## Path fields
Which files contain absolute paths that need rewriting on project move?
```
file: .windsurf/settings.json
field: project_root
```

## References
Docs, source code, or other info about the tool's session format.
