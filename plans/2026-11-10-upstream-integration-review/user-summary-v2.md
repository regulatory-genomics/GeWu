# Interpretation: pseudobulk DE — fibroblast vs microglia (case vs reference)

## ⚠️ Result class: pipeline validation, not biological inference

The DE evidence artifact (`result-d1da3e6685bbea91fa0a5baf6ba97b44--evidence.json`) declares `inference_eligible=false` and `analysis_purpose=pipeline_validation`, with `donor_source=synthetic_random_stratified`. Per its own limitation statement: "Synthetic or declared pipeline-validation data: this run verifies plumbing and numerical execution, not biological-replicate inference." All numbers below should be treated as plumbing/numerical validation of the PyDESeq2 pseudobulk workflow, not as biology of fibroblasts or microglia.

## Analysis setup (from evidence artifact)

- Dataset: `10x_multiome_human_brain_3k_v5`; design `~ cell_type`; backend pydeseq2
- Contrast: `cell_type`, fibroblast (case) vs microglia (reference)
- 4 pseudobulk replicates per condition (fibroblast: 4, microglia: 4)
- Gene filter: min_samples=4, min_total_counts=100 → 3,166 genes tested

## DE summary

- padj ≤ 0.05: 1,636 of 3,166 genes — 806 up in fibroblast, 830 down in fibroblast (up in microglia).

Top genes by padj, **down in fibroblast / up in microglia** (row-level Ensembl IDs from de.tsv):
ENSG00000148655 (log2FC −5.96, padj 7.6e-89), ENSG00000120594 (−3.69), ENSG00000176124 (−4.16), ENSG00000151474 (−3.07), ENSG00000081189 (−4.46), ENSG00000198879 (−3.13), ENSG00000120694 (−2.82), ENSG00000184156 (−3.81), ENSG00000128512 (−2.33), ENSG00000122862 (−6.48).

Top genes by padj, **up in fibroblast** (Ensembl IDs, log2FC, padj):
ENSG00000120885 (+3.08, 2.0e-22), ENSG00000111371 (+4.28, 5.1e-19), ENSG00000131016 (+4.97, 1.1e-18), ENSG00000148672 (+2.83, 2.0e-18), ENSG00000078295 (+2.40, 5.5e-18), ENSG00000158966 (+3.70, 2.2e-16), ENSG00000134376 (+2.52, 4.6e-16), ENSG00000095637 (+3.42, 1.8e-15), ENSG00000152583 (+2.84, 1.8e-15), ENSG00000170624 (+1.88, 3.0e-15).

Note: the de.tsv artifact carries Ensembl gene IDs only (no symbols), so up-gene identities are reported at row level; symbols below come only from the enrichment artifact's gene column.

## Up-gene GO Biological Process enrichment (GO_Biological_Process_2026, over-representation)

10 terms returned; all padj ≤ 0.136, so only the top term survives FDR 0.05:

| Term | Overlap/Set | p | padj |
|---|---|---|---|
| multicellular organism development (GO:0007275) | 17/23 | 5.5e-8 | 1.6e-4 |
| cell-cell adhesion mediated by cadherin (GO:0044331) | 8/10 | 9.7e-5 | 0.136 |
| axon guidance (GO:0007411) | 22/51 | 2.1e-4 | 0.136 |
| GPCR signaling pathway (GO:0007186) | 24/58 | 2.4e-4 | 0.136 |
| cell-cell junction assembly (GO:0007043) | 8/11 | 2.9e-4 | 0.136 |
| positive regulation of epithelial cell proliferation (GO:0050679) | 10/16 | 3.2e-4 | 0.136 |
| cell adhesion (GO:0007155) | 33/91 | 3.5e-4 | 0.136 |
| positive regulation of kinase activity (GO:0033674) | 11/19 | 4.0e-4 | 0.136 |
| cell-cell adhesion (GO:0098609) | 21/50 | 4.6e-4 | 0.136 |
| positive regulation of osteoblast differentiation (GO:0045669) | 12/22 | 4.6e-4 | 0.136 |

Recurring genes across terms: EGFR, FGFR1/2/3, IGF1R, PDGFRB, NTRK2/3, DDR2, ROR1 (receptor tyrosine kinase module); CDH2, CDH4, CDH10, CDH11, CDH20 (cadherin module); BMP6/BMP7, GLI3, YAP1/WWTR1.

Superficially a mesenchymal/adhesion signature, but this must not be over-read given the validation data source.

## Limitations

1. **Pipeline validation data** (`inference_eligible=false`, synthetic random-stratified donors): no biological conclusion is supported.
2. Only 4 pseudobulk replicates per condition and a single-factor `~ cell_type` design; no donor covariates or interaction terms.
3. de.tsv contains Ensembl gene IDs only; no gene symbols in the DE artifact, so top-gene identities cannot be resolved from mounted inputs.
4. Enrichment: only GO:0007275 passes padj < 0.05; the remaining terms are nominal (padj ≈ 0.136). No down-gene enrichment was run.
5. Direction conventions assume positive log2FC = higher in fibroblast (case) per the contrast declaration.

Source artifacts: `result-d1da3e6685bbea91fa0a5baf6ba97b44--{de.tsv,evidence.json,plotly.json}` (node `de`, run-26ddfdb8ceab05c48a9435d991fc6e47) and `result-73caea6118abb1a6e1bb582b79aa4553--{enrichment.tsv,plotly.json}` (node `enrichment`, run-c1f965e461bfb89c82760f57e11f82b3).
