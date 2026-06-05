# ImportStructuredRequest


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `items` | `Vec<ImportStructuredItems>` | :heavy_check_mark: | Files to import (1–100). Pass a single-item list to import one file. Each item is parsed independently; one failure does not abort the others. |
