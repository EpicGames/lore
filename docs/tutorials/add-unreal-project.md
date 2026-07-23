# Add an existing Unreal Engine project to Lore

In this tutorial, you will migrate an existing Unreal Engine or UEFN project from your local file system into a Lore repository. By the end, you'll have a version-controlled project with a proper ignore configuration.

## Prerequisites

- Access to a running Lore server.
- An existing Unreal Engine or UEFN project directory.
- Lore CLI installed and on your PATH.

## Steps

1. **Prepare your local project.**

   Open a terminal in your project root (the folder containing the `.uproject` or `.urc` file).

2. **Run the pre-flight checklist.**

   Before creating the repository, verify what Lore "sees" to avoid accidental tracking of temporary files.

   ```bash
   lore status --scan
   ```

   > [!TIP]
   > Use `lore status --check-dirty` if you are re-connecting a previously tracked project.

3. **Initialize the Lore repository.**

   Connect your local folder to a remote repository URL.

   ```bash
   lore repository create "ucs://<server-address>:<port>/<repo-name>"
   ```

4. **Configure ignore rules.**

   Lore tracks all files by default. You must create a `.loreignore` file in the root directory to exclude temporary build artifacts (like `Intermediate/` or `Saved/`).

   > [!IMPORTANT]
   > Follow the [Configure Loreignore](./loreignore.md) tutorial for an Unreal-specific template.

5. **Commit the ignore file.**

   Commit the `.loreignore` file first so it's active before you stage the rest of the project.

   ```bash
   lore stage .loreignore
   lore commit "Add .loreignore"
   ```

6. **Stage and upload the project.**

   Stage all other files. Lore will automatically filter them based on the `.loreignore` you just committed.

   ```bash
   lore stage .
   lore commit "Initial project upload"
   lore push
   ```

## Verify

Check that your project is stored on the server.

```bash
lore status
```

Expected output:

```text
Local branch in sync with remote
Nothing to commit, working tree clean
```

## Next steps

- [Setting up your Lore Identity](./setup-identity.md)
- [Lore Workflow Basics](./quickstart.md)
