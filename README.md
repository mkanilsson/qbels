# qbels

A simple tree-sitter based lsp implementation for [Quick Backend (QBE) IR](https://c9x.me/compile/).

There is no support for multiple files, everything is on a per-file basis.

## Features

- [X] Rename
- [X] Go to Definition
- [X] Find references
- [ ] Autocomplete
- [ ] Hover (I'd like to show documentation for hovered instruction)

## Installation and Setup

```bash
cargo install qbels
```

### Neovim

With lspconfig:

```lua
require("lspconfig.configs").qbels = {
    default_config = {
        cmd = { "qbels" },
        filetypes = { "qbe", "ssa" },
        root_dir = require("lspconfig").util.root_pattern(".git"),
        settings = {}
    }
}

require("lspconfig").qbels.setup {}
```


### Other

I don't know, if you figure it out, please open a PR and add the instructions here :)
