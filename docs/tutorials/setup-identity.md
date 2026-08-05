# Set up your Lore identity

This tutorial walks you through configuring your user identity in a Lore repository. Your identity (email or username) is attached to every commit you make, allowing collaborators to track changes.

## Prerequisites

- A Lore repository initialized locally (see [Add an existing Unreal Engine project](./add-unreal-project.md)).

## Steps

1. **Verify your current identity.**

   Check what identity Lore is currently using for your local repository.

   ```bash
   lore repository config identity
   ```

   > [!NOTE]
   > If no identity is set, Lore will return an error or a blank value.

2. **Set your repository-level identity.**

   Set the identity for the current project. This value is stored in `.lore/config.toml`.

   ```bash
   lore repository config identity "your-email@example.com"
   ```

3. **Verify the configuration file.**

   Ensure the change was written to your local project configuration.

   ```bash
   cat .lore/config.toml
   ```

## Verify

Confirm your identity is correctly recognized by Lore by checking the status.

```bash
lore status
```

Expected output includes your identity string in the header:

```text
Repository: ...
Identity: your-email@example.com
...
```

## Next steps

- [Add an existing Unreal Engine project to Lore](./add-unreal-project.md)
- [Configure Loreignore](./loreignore.md)
