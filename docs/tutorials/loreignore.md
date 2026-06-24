# Configure loreignore for Unreal Engine

In this tutorial, you will set up a `.loreignore` file tailored for Unreal Engine projects to ensure that only source assets are tracked, while temporary build files and caches are ignored.

## Prerequisites

- A Lore repository initialized locally (see [Add an existing Unreal Engine project](./add-unreal-project.md)).

## Steps

1. **Create the .loreignore file.**

   In your project root, create a new file named `.loreignore`.

   ```bash
   touch .loreignore
   ```

2. **Add Unreal Engine ignore patterns.**

   Add the following patterns to the file. These cover standard Unreal Engine temporary folders and local user settings.

   ```text
   # Unreal Engine temporary folders
   Binaries/
   Build/
   DerivedDataCache/
   Intermediate/
   Saved/

   # Local user settings
   .vscode/
   .idea/
   *.sln
   *.suo
   
   # UEFN specific (if applicable)
   .urc/
   ```

3. **Verify the ignore rules.**

   Use `lore status --scan` to verify that the folders listed above are no longer being tracked by Lore.

   ```bash
   lore status --scan
   ```

4. **Verify the configuration file.**

   Ensure the change was written to your local project configuration.

   ```bash
   cat .lore/config.toml
   ```

## Verify

Check that the ignored directories don't appear in the staged or untracked file list.

```bash
lore status
```

Expected output:

```text
Changes not staged for commit:
  (use "lore stage <file>..." to update what will be committed)
        modified:   .loreignore
```

## Next steps

- [Stage and commit your project](./add-unreal-project.md#steps)
- [Set up your Lore identity](./setup-identity.md)
