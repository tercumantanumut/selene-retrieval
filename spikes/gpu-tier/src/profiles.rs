// TierProfile table (included by tier.rs). Revisions and sha256 are the HF LFS values verified
// 2026-10-17 against the files benchmarked in spikes/gpu-tier/models (sha256sum matches).

const BGE_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

pub const BGE_SMALL_FP32: ModelFile = ModelFile {
    hf_repo: "Xenova/bge-small-en-v1.5",
    hf_revision: "ea104dacec62c0de699686887e3f920caeb4f3e3",
    path: "onnx/model.onnx",
    bytes: 133_093_490,
    precision: Precision::Fp32,
    sha256: "828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35",
};
pub const BGE_SMALL_FP16: ModelFile = ModelFile {
    hf_repo: "Xenova/bge-small-en-v1.5",
    hf_revision: "ea104dacec62c0de699686887e3f920caeb4f3e3",
    path: "onnx/model_fp16.onnx",
    bytes: 66_749_212,
    precision: Precision::Fp16,
    sha256: "4d76223b73a3fd708f009a93a830a6ed5e536b07c77d3c812ba7b35a4988d767",
};
pub const BGE_SMALL_INT8: ModelFile = ModelFile {
    hf_repo: "Xenova/bge-small-en-v1.5",
    hf_revision: "ea104dacec62c0de699686887e3f920caeb4f3e3",
    path: "onnx/model_int8.onnx",
    bytes: 33_760_831,
    precision: Precision::Int8,
    sha256: "bf64d05457cb391fa88d045faf5927a15ea36d96228ddf23ea970087afdc1197",
};
pub const BGE_SMALL_Q_OPT: ModelFile = ModelFile {
    hf_repo: "Qdrant/bge-small-en-v1.5-onnx-Q",
    hf_revision: "aa8f8b060edb00e03bfdd08813a2949946c8ba55",
    path: "model_optimized.onnx",
    bytes: 66_465_124,
    precision: Precision::Fp32,
    sha256: "51f1bd0addd6e859e42c2c8021a5e5461385bb676a649f4b269aa445449f2431",
};
pub const ARCTIC_M15_FP16: ModelFile = ModelFile {
    hf_repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
    hf_revision: "e58a8f756156a1293d763f17e3aae643474e9b8a",
    path: "onnx/model_fp16.onnx",
    bytes: 218_155_826,
    precision: Precision::Fp16,
    sha256: "8384cc8a480d60ade695631ced7b015b3f21d959a8ad3cdffd189a4005408c98",
};
pub const ARCTIC_M15_INT8: ModelFile = ModelFile {
    hf_repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
    hf_revision: "e58a8f756156a1293d763f17e3aae643474e9b8a",
    path: "onnx/model_int8.onnx",
    bytes: 110_145_162,
    precision: Precision::Int8,
    sha256: "a18f437b2466863901a0bdc14904cf93246f5ecce0b656fc773bc2b7b2f84f6e",
};
pub const ARCTIC_L_FP16: ModelFile = ModelFile {
    hf_repo: "Snowflake/snowflake-arctic-embed-l",
    hf_revision: "d8fb21ca8d905d2832ee8b96c894d3298964346b",
    path: "onnx/model_fp16.onnx",
    bytes: 668_828_773,
    precision: Precision::Fp16,
    sha256: "8ca24235754e5553234266fba529a7eec363e8d565d3f21761ada22841f5f3c8",
};
pub const ARCTIC_L_INT8: ModelFile = ModelFile {
    hf_repo: "Snowflake/snowflake-arctic-embed-l",
    hf_revision: "d8fb21ca8d905d2832ee8b96c894d3298964346b",
    path: "onnx/model_int8.onnx",
    bytes: 336_984_086,
    precision: Precision::Int8,
    sha256: "ab802115224b05d283e2042cf889b5490c476e19d28ce93e2073a57476b95541",
};

pub fn profile(tier: Tier) -> TierProfile {
    match tier {
        Tier::Minimum => TierProfile {
            tier,
            model_id: "BAAI/bge-small-en-v1.5",
            dims: 384,
            pooling: "cls",
            query_prefix: BGE_PREFIX,
            max_seq_tokens: 512,
            // Both bundled (95 MB): int8 is CPU-pinned (DML int8 != CPU int8, cos 0.997), fp16 is EP-portable.
            gpu_file: BGE_SMALL_FP16,
            cpu_file: BGE_SMALL_INT8,
            batch_gpu: 64,
            batch_cpu: 8,
            concurrency_gpu: 2,
            concurrency_cpu: 1,
            ann: AnnParams {
                min_rows: 50_000,
                partitions_per_sqrt_rows: 1.0,
                hnsw_m: 16,
                hnsw_ef_construction: 100,
                nprobes: 20,
                refine_factor: 5,
            },
            rerank: false,
            rerank_top_n: 0,
        },
        Tier::Standard => TierProfile {
            tier,
            model_id: "Snowflake/snowflake-arctic-embed-m-v1.5",
            dims: 768,
            pooling: "cls",
            query_prefix: BGE_PREFIX,
            max_seq_tokens: 512,
            gpu_file: ARCTIC_M15_FP16,
            cpu_file: ARCTIC_M15_INT8,
            batch_gpu: 64,
            batch_cpu: 8,
            concurrency_gpu: 2,
            concurrency_cpu: 1,
            ann: AnnParams {
                min_rows: 50_000,
                partitions_per_sqrt_rows: 1.0,
                hnsw_m: 20,
                hnsw_ef_construction: 150,
                nprobes: 24,
                refine_factor: 5,
            },
            rerank: false,
            rerank_top_n: 0,
        },
        Tier::High => TierProfile {
            tier,
            model_id: "Snowflake/snowflake-arctic-embed-l",
            dims: 1024,
            pooling: "cls",
            query_prefix: BGE_PREFIX,
            max_seq_tokens: 512,
            gpu_file: ARCTIC_L_FP16,
            cpu_file: ARCTIC_L_INT8,
            batch_gpu: 32,
            batch_cpu: 8,
            concurrency_gpu: 2,
            concurrency_cpu: 1,
            ann: AnnParams {
                min_rows: 50_000,
                partitions_per_sqrt_rows: 1.0,
                hnsw_m: 24,
                hnsw_ef_construction: 200,
                nprobes: 32,
                refine_factor: 10,
            },
            rerank: true,
            rerank_top_n: 50,
        },
    }
}
