# Pseudobulk DE interpretation: fibroblast vs microglia (dataset `10x_multiome_human_brain_3k_v5`)

## ⚠️ Pipeline validation, not biological inference
The DE evidence artifact reports `analysis_purpose: pipeline_validation` and `inference_eligible: false`, with donors from `synthetic_random_stratified`. Per the artifact's own limitation: this run verifies plumbing and numerical execution, **not** biological-replicate inference. All results below must be read as pipeline-validation output.

## Design and counts
- Backend: PyDESeq2, design `~ cell_type`, contrast `cell_type: fibroblast vs microglia`; 4 pseudobulk replicates per condition.
- Gene filter: min 4 samples, min 100 total counts → 3,166 genes with numeric `padj`.
- Significant at padj ≤ 0.05: **1,636** genes (**806 up** in fibroblast, **830 down**).

## Top upregulated genes (fibroblast > microglia), by adjusted p-value
Row-level IDs only, from the DE table (de node):

| gene_id | log2FC | padj |
|---|---|---|
| ENSG00000120885 | 3.08 | 2.02e-22 |
| ENSG00000111371 | 4.28 | 5.11e-19 |
| ENSG00000131016 | 4.97 | 1.09e-18 |
| ENSG00000148672 | 2.83 | 1.98e-18 |
| ENSG00000078295 | 2.40 | 5.54e-18 |
| ENSG00000158966 | 3.70 | 2.16e-16 |
| ENSG00000134376 | 2.52 | 4.63e-16 |
| ENSG00000095637 | 3.42 | 1.80e-15 |
| ENSG00000152583 | 2.84 | 1.85e-15 |
| ENSG00000170624 | 1.88 | 3.03e-15 |

Largest positive log2FC among significant up genes: ENSG00000137801 (+6.52, padj 2.85e-3), ENSG00000187498 (+6.41), ENSG00000103888 (+5.84). The strongest overall signals by padj are strongly *down* in fibroblast (e.g., ENSG00000148655, log2FC −5.96, padj 7.6e-89) — a coherent cell-identity expression split.

## Up-gene GO Biological Process enrichment (over-representation)
Top term by p-value: **multicellular organism development** (GO:0007275; overlap 17/23, p = 5.47e-8, padj = 1.62e-4) — the only term surviving FDR 0.05. Other nominally enriched terms (all padj = 0.136, not significant after correction): cadherin-mediated cell-cell adhesion (8/10), axon guidance (22/51), GPCR signaling (24/58), cell-cell junction assembly (8/11), positive regulation of epithelial cell proliferation (10/16), cell adhesion (33/91), positive regulation of kinase activity (11/19), cell-cell adhesion (21/50), positive regulation of osteoblast differentiation (12/22). Gene symbols come verbatim from the enrichment table (enrichment node).

## Limitations
- Synthetic stratified donors; results validate the pipeline, not biology — no biological conclusion should be drawn about fibroblast–microglia differences in real brain tissue.
- Genes are reported as Ensembl IDs only (the DE table has no symbol column); symbol-level interpretation relies solely on the enrichment table.
- GO BP results are from over-representation of up genes; only GO:0007275 passes FDR ≤ 0.05; the remaining terms are nominal.
- Small n per condition (4 pseudobulk replicates each).