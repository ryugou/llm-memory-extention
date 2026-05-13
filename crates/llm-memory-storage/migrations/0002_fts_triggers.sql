CREATE VIRTUAL TABLE raws_fts USING fts5(
  title, content, tags,
  content='raws', content_rowid='rowid'
);

CREATE TRIGGER raws_ai AFTER INSERT ON raws BEGIN
  INSERT INTO raws_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;
CREATE TRIGGER raws_ad AFTER DELETE ON raws BEGIN
  INSERT INTO raws_fts(raws_fts, rowid, title, content, tags)
  VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;
CREATE TRIGGER raws_au AFTER UPDATE ON raws BEGIN
  INSERT INTO raws_fts(raws_fts, rowid, title, content, tags)
  VALUES ('delete', old.rowid, old.title, old.content, old.tags);
  INSERT INTO raws_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;
