# perspica-core

The analysis engine behind [perspica](https://github.com/sshah03/perspica). It takes the old and new source of changed files and works out what changed: renames, moves, signature changes, mechanical noise, the reading order and which tests reach the changed code. It doesn't touch git, the terminal or the network.

To review code changes, install the tool instead:

```bash
cargo install perspica
```
