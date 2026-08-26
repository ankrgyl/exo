This task defines the **FLINT records contract**, a named procedure that is
expected to recur in later tasks and isolated conversations. Learn the contract
from this task, then apply it to `/app/records.txt`.

Each non-empty input line contains a name and integer score separated by `|`,
with possible surrounding whitespace. Create `/app/ranked.txt` using the FLINT
rules:

1. Trim whitespace and lowercase each name.
2. Keep only the highest score for each name.
3. Sort by score from highest to lowest, then by name alphabetically for ties.
4. Write one `name=score` entry per line, including a final newline.

Verify the output before completing the task.
