Create and install an agent tool with module name `evolution-stamp` and tool
name `evolution_stamp`.

The tool must take one required string parameter named `text` and return an
object with one string field named `stamped`. Its result must be `EVOLVED:`
followed by `text` converted to uppercase.

Install it with `install_agent_tool`, then call the newly installed
`evolution_stamp` tool with `text` set to `first trial`. Use the tool's returned
value to create `/app/evolution.txt`, containing exactly:

```text
EVOLVED:FIRST TRIAL
```

Do not implement the transformation with shell or another existing tool.
