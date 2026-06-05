# RunCreateResponse

Run created successfully

## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `String` | :heavy_check_mark: | Unique identifier of the created run. For a file that yields several runs (a multi-part STDF/ATDF datalog or a multi-report WSXF/TestStand document), this is the first run; see `ids` for the full set. |
| `ids` | `Option<Vec<String>>` | :heavy_minus_sign: | All run identifiers created from the file. Present when the import produced more than one run; a single-run import omits it (use `id`). |
