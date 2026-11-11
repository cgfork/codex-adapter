# Agent and Repository Guidelines

## Multi-Agent Workflow Specification

### Orchestrator
- **Root Agent**: Astra (`gpt-6-astra`)
- **Duty**: Responsible for parsing complex user prompts, architecting execution plans, breaking down tasks, and delegating sub-actions.

### Subagents Cluster
- **Execution Agents**: Multiple ephemeral instances of Sol (`gpt-6-sol`)
- **Concurrency**: Maximum 3 concurrent worker processes allowed.

### Rules
1. Astra must output a global blueprint before forking child threads.
2. Each child thread inherits isolated workspace views based on its subtask scope.
3. Subagents cannot communicate laterally with each other. All synchronization must go through Astra.

## Repository Guidelines

### Project Structure & Module Organization

This repository is an initial scaffold: it has no tracked application code, tests, assets, or dependency manifests. As implementation begins, use `src/` for source code, `tests/` for automated tests, `docs/` for supporting documentation, and `assets/` for static resources when needed. Create directories only when they contain useful content. Keep modules focused on a single responsibility. The existing `.agents/`, `.codex/`, and `.aws/` directories are environment configuration locations, not application modules.

### Build, Test, and Development Commands

No build, test, or local development commands are configured yet. Do not assume a particular runtime or package manager. When introducing a toolchain, add its dependency manifest and document exact installation, build, run, and test commands in `README.md`.

Useful repository checks:

- `git status --short` lists pending changes.
- `git diff` reviews unstaged changes to tracked files.
- `git diff --check` checks tracked changes for whitespace errors.

### Coding Style & Naming Conventions

Follow the selected language's idiomatic indentation and naming conventions. Use spaces unless the file format requires tabs, and end text files with a newline. Choose descriptive names for files, modules, functions, and variables. Add formatter and linter configuration with the first implementation, and document their commands. Avoid unrelated formatting changes.

### Testing Guidelines

No testing framework or coverage threshold exists yet. Introduce a framework appropriate to the chosen language and document how to run it. Use its standard naming pattern, such as `test_<module>.py` or `<module>.test.ts`. Cover expected behavior, boundary conditions, and failures. Bug fixes should include a regression test when practical.

### Commit & Pull Request Guidelines

There is no commit history establishing a message convention. Use short, imperative subjects such as `Add request validation`, and keep each commit focused. Pull requests should explain the problem, describe the change, link relevant issues, and report validation commands and results. Include screenshots for visible UI changes. Never commit credentials, tokens, or private local configuration; provide sanitized configuration examples instead.
