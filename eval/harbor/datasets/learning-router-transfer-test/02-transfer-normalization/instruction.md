Read `/app/records.txt`. Each non-empty line contains a name and integer score
separated by `|`, with possible surrounding whitespace.

Create `/app/ranked.txt` using these rules:

1. Trim whitespace and lowercase each name.
2. Keep only the highest score for each name.
3. Sort by score from highest to lowest, then by name alphabetically for ties.
4. Write one `name=score` entry per line, including a final newline.

Verify the output before completing the task.
