
Claude Code plugin hooks report the conversation's resume command as `tollgate resume <sid> --profile <account>` on SessionStart and attribution changes. Reports use a persisted increasing sequence; failed reports leave herdr's existing resume plan in place.
