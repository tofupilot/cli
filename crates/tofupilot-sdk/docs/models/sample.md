# Sample

Sample classification. 'golden' marks a known-good reference unit; 'failing' marks a known-faulty reference unit; 'ignored' marks a bench-check unit excluded from analytics and alerts. All are excluded from production analytics aggregates (FPY, Cpk, throughput) by default. Omit or null for regular production units.

## Values

| Variant | Wire Value |
| --- | --- |
| `Sample::Golden` | `golden` |
| `Sample::Failing` | `failing` |
| `Sample::Ignored` | `ignored` |
