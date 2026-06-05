# ImportStructuredItems


## Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `upload_id` | `String` | :heavy_check_mark: | ID of a previously uploaded file (via Initialize and Finalize upload). |
| `importer` | `ImportStructuredImporter` | :heavy_check_mark: | Source format of the uploaded file. OPENHTF for OpenHTF JSON logs; WATS for Virinco WATS WSJF (JSON); WSXF for WATS WSXF (XML); ATML for IEEE 1671 ATML Test Results (XML); TESTSTAND for NI TestStand native XML reports; STDF for binary STDF V4 datalogs; ATDF for ATDF (the ASCII text form of STDF). For CSV/tabular files use the dedicated tabular import endpoint. |
