---
name: genome-resource-canvas
description: Use when registering genomic assay resources such as ATAC UCF files and opening Genome Canvas/Tachyon at a locus.
required_skills:
  - resource-registration-validator
capabilities:
  - resource.search
  - resource.validate
  - resource.register
  - gene.resolve_symbol
  - canvas.open_genome
  - canvas.list
---

# Genome Resource Canvas SOP

Use this skill when the user asks to register a genomic data file, sample, track, assay, or open Genome Canvas at a gene/locus.

**IMPORTANT**: Before registering UCF/ATAC resources, follow the `resource-registration-validator` skill to enforce validation rules.

## Strict Validation Rules

### UCF/ATAC Conventions (MUST FOLLOW)

1. **Forbid filename-as-sample-id**: If the `sample_id` matches the filename basename (e.g., `P1-2_ATAC3.filtered.ucf`), derive the sample ID from the parent directory or ask the user for clarification.
2. **Require `assay=atac` for UCF files**: All `.ucf` track resources MUST have `"assay": "atac"` in their labels.
3. **Require `source=coverage` for UCF**: All ATAC signal tracks MUST have `"source": "coverage"` in their track definition.
4. **Report missing registry entries**: Before opening Genome Canvas, verify sample and track URIs exist via `resource.search`. If not found, report the missing entries and register them first.

## Workflow

1. **Inspect the user-provided path** with file tools. If access is denied, report the denied path and allowed roots; ask for `GEWU_READ_ROOTS` configuration rather than guessing files.
2. **Apply resource-registration-validator rules** to validate conventions before registration.
3. **Search registry** for existing entries:

   ```json
   {
     "query": "gewu://sample/ucf_atac_hg38",
     "kind": "sample"
   }
   ```

   If the sample already exists, skip registration. If not found, proceed to step 4.

4. **Register the sample resource** (only if not found):

   ```json
   {
     "resource": {
       "uri": "gewu://sample/ucf_atac_hg38",
       "kind": "sample",
       "name": "UCF ATAC hg38",
       "aliases": ["UCF", "ucf", "UCF_ATAC", "P1-2_ATAC3.filtered.ucf"],
       "labels": {"assay": "atac", "assembly": "hg38", "sample_id": "UCF"},
       "spec": {"path": "/path/to/P1-2_ATAC3.filtered.ucf"},
       "status": "available"
     },
     "mode": "upsert",
     "reason": "Register UCF ATAC hg38 sample for Genome Canvas"
   }
   ```

5. **Register the track resource** (only if not found):

   ```json
   {
     "resource": {
       "uri": "gewu://track/ucf_atac_signal_hg38",
       "kind": "track",
       "name": "UCF ATAC signal hg38",
       "aliases": ["UCF ATAC track", "ucf_atac_signal", "P1-2_ATAC3.filtered.ucf"],
       "labels": {"assay": "atac", "assembly": "hg38", "sample_id": "UCF", "track_type": "signal"},
       "spec": {"path": "/path/to/P1-2_ATAC3.filtered.ucf"},
       "status": "available"
     },
     "mode": "upsert",
     "reason": "Register UCF ATAC signal track for Genome Canvas"
   }
   ```

   **CRITICAL**: Track definition MUST include `"source": "coverage"` for ATAC signal tracks.

6. **Resolve gene symbols** with `gene.resolve_symbol` and use the returned primary hg38 region.
7. **Open Genome Canvas** with `canvas.open_genome`:

   ```json
   {
     "assembly": "hg38",
     "region": "chr11:47354860-47395607",
     "gene_symbol": "SPI1",
     "title": "ATAC UCF hg38 — SPI1",
     "samples": [{"id": "UCF", "label": "UCF", "assay": "atac", "uri": "gewu://sample/ucf_atac_hg38"}],
     "tracks": [{
       "id": "signal-atac-UCF",
       "type": "signal",
       "source": "coverage",
       "sample_id": "UCF",
       "assay": "atac",
       "uri": "gewu://track/ucf_atac_signal_hg38",
       "path": "/path/to/P1-2_ATAC3.filtered.ucf",
       "url": "/path/to/P1-2_ATAC3.filtered.ucf"
     }]
   }
   ```

8. **Verify with `canvas.list`**. If the backend canvas exists but the user cannot see it, inspect canvas frontend hydration/renderer status; do not re-register resources as a first response.

## Validation Failure Handling

| Failure | Action |
|---------|--------|
| `sample_id` matches filename | Reject registration, ask user to confirm sample ID |
| Missing `assay=atac` on UCF | Reject registration, add `assay: atac` to labels |
| Missing `source=coverage` on ATAC track | Reject registration, add `source: coverage` to track |
| URI not found in registry | Report which URIs are missing before opening canvas |
