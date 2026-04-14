Create an isolated git worktree for parallel Claude Code sessions.

## Steps

1. Get the current branch name: `git branch --show-current`
2. Generate a worktree name: `worktree-<branch>-<timestamp>`
3. Create the worktree:
   ```
   git worktree add ../tickvault-<N> -b worktree/<branch>-<N>
   ```
   Where `<N>` is 1, 2, 3... (find next available)
4. Print instructions:
   ```
   Worktree created at: ../tickvault-<N>
   Branch: worktree/<branch>-<N>

   To start a parallel Claude session:
     cd ../tickvault-<N>
     claude

   When done, merge changes back:
     git checkout <original-branch>
     git merge worktree/<branch>-<N>

   To clean up:
     git worktree remove ../tickvault-<N>
     git branch -d worktree/<branch>-<N>
   ```
5. List all active worktrees: `git worktree list`

## Rules

- Never create worktrees inside the current repo directory
- Always create a new branch for the worktree
- Name branches with `worktree/` prefix for easy identification
