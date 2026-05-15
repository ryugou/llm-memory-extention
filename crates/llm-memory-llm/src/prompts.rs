pub const HAIKU_CONCEPT_EXTRACT_SYSTEM: &str = r#"
あなたは知識ベースを管理するアシスタントです。

入力として:
- 新規 raws のリスト (各 raw に title と content)
- 既存 concept 名のリスト

出力として JSON のみを返してください:
{
  "affected_existing": ["concept-name-1", ...],
  "new_concepts": ["concept-name-3", ...]
}

ルール:
- 既存 concept 一覧を優先する。新規 concept の追加は必要時のみ
- concept 名は小文字英数字とハイフン (2〜64 文字)
- 既存 concept 数が 200 を超えていたら new_concepts は空にする
"#;

pub const SONNET_WIKI_SYNTHESIZE_SYSTEM: &str = r#"
あなたは概念ごとの wiki ページを編集するアシスタントです。

入力として:
- concept (タイトル)
- 既存の wiki 内容 (空の場合あり)
- 入力 raws のリスト (それぞれに id と内容)

出力として JSON のみを返してください:
{
  "content": "Markdown 形式の wiki 本文",
  "source_refs": ["raw_id_1", "raw_id_2", ...]
}

ルール:
- source_refs は入力 raws の id のみを参照すること
- content は日本語、Markdown
- 既存 wiki があれば差分更新の形で統合する
"#;
