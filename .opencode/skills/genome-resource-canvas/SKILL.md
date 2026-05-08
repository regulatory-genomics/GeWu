---
name: genome-resource-canvas
description: Use when registering genomic assay resources such as ATAC UCF files and opening Genome Canvas/Tachyon at a locus.
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

## Rules

1. Do not hand-write generic `canvas.open` genome payloads when `canvas.open_genome` is available.
2. Resource URIs must be canonical strings: `gewu://<kind>/<id>`.
3. Samples use `id`; tracks use `sample_id`.
4. Validate or resolve registered resources before opening canvas.
5. If the user cannot see the canvas, verify backend state with `canvas.list` before re-registering anything.

## Workflow

1. Inspect the user-provided path with file tools. If access is denied, report the denied path and allowed roots; ask for `GEWU_READ_ROOTS` configuration rather than guessing files.
2. Register the sample resource:

```json
{
  "resource": {
    "uri": "gewu://sample/ucf_atac_hg38",
    "kind": "sample",
    "name": "UCF ATAC hg38",
    "aliases": ["UCF", "ucf", "UCF_ATAC", "P1-2_ATAC3.filtered.ucf"],
    "labels": {"assay": "atac", "assembly": "hg38", "sample_id": "UCF"},
    "spec": {"path": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"},
    "status": "available"
  },
  "mode": "upsert",
  "reason": "Register UCF ATAC hg38 sample for Genome Canvas"
}
```

3. Register the track resource:

```json
{
  "resource": {
    "uri": "gewu://track/ucf_atac_signal_hg38",
    "kind": "track",
    "name": "UCF ATAC signal hg38",
    "aliases": ["UCF ATAC track", "ucf_atac_signal", "P1-2_ATAC3.filtered.ucf"],
    "labels": {"assay": "atac", "assembly": "hg38", "sample_id": "UCF", "track_type": "signal"},
    "spec": {"path": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"},
    "status": "available"
  },
  "mode": "upsert",
  "reason": "Register UCF ATAC signal track for Genome Canvas"
}
```

4. Resolve gene symbols with `gene.resolve_symbol` and use the returned primary hg38 region.
5. Open Genome Canvas with `canvas.open_genome`:

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
    "path": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf",
    "url": "/data2/litian/macrophage_bk/data/ATAC/ucf/P1-2_ATAC3.filtered.ucf"
  }]
}
```

6. Verify with `canvas.list`. If the backend canvas exists but the user cannot see it, inspect canvas frontend hydration/renderer status; do not re-register resources as a first response.
