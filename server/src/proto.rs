// tonic が build.rs で生成する graphrag.rs をそのまま取り込む。生成コードの doc コメントは
// リスト項目のインデントが clippy::doc_overindented_list_items に触れるが、生成物は手で直せない
// ため、この取り込みモジュール境界で当該 lint だけ許可する（自作コードには影響しない）。
#[allow(clippy::doc_overindented_list_items)]
pub mod graphrag {
    tonic::include_proto!("graphrag");
}
