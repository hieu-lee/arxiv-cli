//! Core data types shared by every module.

/// Journal/conference classification. `q` is 1..=4 (SJR-style quartile; conferences mapped from CORE rank).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Venue {
    pub name: &'static str,
    pub q: u8,
    pub area: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct Paper {
    /// Versionless id: "2301.01234" or "math/0309285".
    pub id: String,
    /// Latest version number (>=1), 0 if unknown.
    pub ver: u32,
    /// Whitespace-normalised.
    pub title: String,
    pub authors: Vec<String>,
    /// Affiliations given in arXiv metadata (deduped; usually empty).
    pub affs: Vec<String>,
    /// Whitespace-normalised abstract.
    pub abs: String,
    /// cats[0] is the primary category.
    pub cats: Vec<String>,
    /// ISO-8601 "2026-10-09T14:14:14Z" (first version).
    pub published: String,
    /// ISO-8601 (latest version).
    pub updated: String,
    /// arXiv author comment ("12 pages, accepted to ...").
    pub comment: Option<String>,
    /// Author-supplied journal reference.
    pub jref: Option<String>,
    pub doi: Option<String>,
    /// MSC classes (filled by enrichment from the abs page; empty otherwise).
    pub msc: Vec<String>,
    /// Resolved from jref/comment by `venue`.
    pub venue: Option<Venue>,
    /// Institutions extracted from HTML full text (filled by enrichment; empty otherwise).
    pub insts: Vec<String>,
    /// Enrichment already attempted: bit 1 = insts, bit 2 = msc (so display never refetches).
    pub enriched: u8,
}

impl Paper {
    /// "YYYY-MM-DD" of first submission.
    pub fn date(&self) -> &str {
        self.published.get(..10).unwrap_or(&self.published)
    }
    pub fn updated_date(&self) -> &str {
        self.updated.get(..10).unwrap_or(&self.updated)
    }
    pub fn year(&self) -> &str {
        self.published.get(..4).unwrap_or("")
    }
    pub fn primary(&self) -> &str {
        self.cats.first().map(String::as_str).unwrap_or("")
    }
    /// Has journal_ref or DOI (i.e. the authors report a publication).
    pub fn is_published(&self) -> bool {
        self.jref.is_some() || self.doi.is_some()
    }
    pub fn url(&self) -> String {
        format!("https://arxiv.org/abs/{}", self.id)
    }
}

/// Kind of a theorem-like block. `code()` is the value accepted by `--kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Problem,
    Conjecture,
    Question,
    Exercise,
    Theorem,
    Lemma,
    Proposition,
    Corollary,
    Definition,
    Remark,
    Example,
    Claim,
    Other,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Kind::Problem => "prob",
            Kind::Conjecture => "conj",
            Kind::Question => "q",
            Kind::Exercise => "ex",
            Kind::Theorem => "thm",
            Kind::Lemma => "lem",
            Kind::Proposition => "prop",
            Kind::Corollary => "cor",
            Kind::Definition => "def",
            Kind::Remark => "rem",
            Kind::Example => "exm",
            Kind::Claim => "cl",
            Kind::Other => "oth",
        }
    }
}

/// A theorem-like block extracted from a paper (text is Markdown with `$latex$` math).
#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    /// As printed: "Problem 3", "Conjecture 1.2", "Theorem" ...
    pub label: String,
    /// Section id the item lives in ("2.1"), "" if none.
    pub sec: String,
    pub text: String,
}

/// Heading in a document. `start..end` is the byte range of the section body inside the
/// document's text (Markdown for HTML docs, raw LaTeX for tex docs), heading line included.
#[derive(Clone, Debug)]
pub struct Section {
    /// Hierarchical id: "1", "1.2", "A" ...; sequential "s1.." when unnumbered.
    pub id: String,
    pub level: u8,
    pub title: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct Ref {
    pub n: u32,
    pub text: String,
    pub arxiv: Option<String>,
    pub doi: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub sec: String,
    /// Byte offset in the searched text.
    pub pos: usize,
    pub snippet: String,
}
