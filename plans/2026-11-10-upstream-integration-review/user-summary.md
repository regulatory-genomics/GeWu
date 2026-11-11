# Interpretation: pseudobulk DE — fibroblast vs microglia (biological replicates)

## Scope of evidence
- **Case:** fibroblast; **Reference:** microglia; contrast: fibroblast vs microglia (positive log2FC = higher in fibroblast).
- Inputs: `tachyon.de.result.v1` (PyDESeq2-style pseudobulk DE table, 3,166 rows, Ensembl gene IDs, GRCh38) and `tachyon.enrichment.result.v1` (GO_Biological_Process_2026 over-representation, up direction, 20 terms). Volcano and enrichment Plotly figures were declared but not numerically re-read here.

## Differential expression summary (row-level, from the DE table)
- **3,166 genes tested; 1,636 significant at padj ≤ 0.05** (806 up in fibroblast, 830 down in fibroblast — a roughly balanced large-scale transcriptional difference between these cell types).
- Effect sizes are strong: among significant genes, 693 up-genes have log2FC > 1 and 707 down-genes have log2FC < −1.
- The **strongest evidence is for down-regulation in fibroblast** (i.e., microglia-enriched genes): the top 10 by padj are all negative-log2FC: ENSG00000148655 (−5.96, padj 7.6e-89), ENSG00000120594 (−3.69, 2.7e-72), ENSG00000176124 (−4.16, 3.5e-47), ENSG00000151474 (−3.07, 2.6e-45), ENSG00000081189 (−4.46, 2.6e-45), ENSG00000198879 (−3.13, 2.4e-44), ENSG00000120694 (−2.82, 7.1e-43), ENSG00000184156 (−3.81, 3.9e-41), ENSG00000128512 (−2.33, 1.8e-40), ENSG00000122862 (−6.48, 3.0e-39). This is consistent with loss of microglial identity genes in fibroblasts (the DE table carries only Ensembl IDs, so no symbol-level claims are made here).

## Top upregulated genes in fibroblast (from the DE table rows)
Most extreme log2FC (padj ≤ 0.05):
| gene_id | log2FC | padj |
|---|---|---|
| ENSG00000137801 | 6.52 | 2.9e-3 |
| ENSG00000187498 | 6.42 | 7.8e-9 |
| ENSG00000153162 | 6.10 | 1.7e-4 |
| ENSG00000103888 | 5.84 | 3.3e-15 |
| ENSG00000103196 | 5.39 | 4.6e-8 |
| ENSG00000137962 | 5.38 | 4.3e-11 |
| ENSG00000112837 | 5.25 | 1.2e-5 |
| ENSG00000135862 | 5.01 | 4.9e-7 |
| ENSG00000131016 | 4.97 | 1.1e-18 |
| ENSG00000163453 | 4.61 | 7.0e-6 |

Most significant by padj (all up): ENSG00000120885 (log2FC 3.08, padj 2.0e-22), ENSG00000111371 (4.28, 5.1e-19), ENSG00000131016 (4.97, 1.1e-18), ENSG00000148672 (2.83, 2.0e-18), ENSG00000078295 (2.40, 5.5e-18), ENSG00000158966 (3.70, 2.2e-16), ENSG00000134376 (2.52, 4.6e-16), ENSG00000095637 (3.42, 1.8e-15), ENSG00000152583 (2.84, 1.8e-15), ENSG00000170624 (1.88, 3.0e-15).

## GO Biological Process enrichment (up-genes, GO_Biological_Process_2026)
The enrichment artifact contains **20 up-direction terms**; key rows:

- **Multicellular organism development** (GO:0007275) — the only term surviving BH padj ≤ 0.05 (padj 1.62e-4; p = 5.47e-8; 17/23 overlap). Genes: EGFR, NTRK3, IGF1R, PDGFRB, CDH4, ERBB4, FGFR2, DDR2, FGFR3, CDHR3, ROR1, CDH2, CDH20, NTRK2, FGFR1, CDH10, CDH11 — dominated by receptor tyrosine kinase, FGF/EGF/DDR and cadherin families, coherent with a stromal/mesenchymal fibroblast program.
- Below-padj terms (p < 5e-4, padj = 1.36e-1) repeatedly center on **adhesion/junction biology** — cell adhesion (GO:0007155, 33/91), cell-cell adhesion (GO:0098609, 21/50), cadherin-mediated adhesion (GO:0044331), cell-cell junction assembly (GO:0007043) — and on **signaling**: axon guidance (GO:0007411, 22/51), GPCR signaling (GO:0007186, 24/58), positive regulation of kinase activity (GO:0033674, 11/19), positive regulation of epithelial cell proliferation (GO:0050679, 10/16), positive regulation of osteoblast differentiation (GO:0045669, 12/22). Recurring genes: CDH family, LAMA1/2/4, LAMC1, TNC, COL6A3, FERMT2, VCL, and FGFR/EGFR/BMP/YAP–TAZ modules.
- **Interpretation:** the fibroblast-up transcriptome is broadly mesenchymal — developmental/adhesion/ECM-matrix signaling — while the microglia-up half carries the genome-wide most significant signal (microglial identity genes lost in fibroblasts).

## Limitations
- The DE table reports only Ensembl gene IDs; **no gene symbols** are available in that artifact, so top-gene lists above are symbol-free by design. Symbol names appear only in the enrichment table's contributing-gene lists.
- The enrichment analysis was run on **up-genes only** (no down-gene enrichment in the mounted artifact); all sub-threshold excerpted terms share padj = 1.36e-1, so only GO:0007275 is robust after multiple-testing correction within this set.
- Enrichment `genes` columns list contributing symbols from the artifact; overlaps are term rows, not necessarily distinct-gene counts.
- Neither artifact carries replicate counts, sample sizes, or QC metrics; replicate structure ("biological-replicate pseudobulk") is asserted by upstream provenance, not verifiable here.
- Figures (volcano, enrichment plot) were declared but not parsed for numbers; all quantitative statements come from the two TSVs.