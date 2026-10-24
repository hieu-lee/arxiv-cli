pub struct Tag {
    pub name: &'static str,
    pub desc: &'static str,
    /// arXiv search_query fragment, e.g. `(ti:"diffusion model" OR ti:"diffusion models")`.
    pub query: &'static str,
}

macro_rules! tag {
    ($n:expr, $d:expr, $q:expr) => {
        Tag { name: $n, desc: $d, query: $q }
    };
}

/// Topic presets spanning arXiv. Each query is a parenthesised search_query fragment.
static TAGS: &[Tag] = &[
    tag!("llm", "large language models", r#"(ti:"large language model" OR ti:"large language models" OR ti:LLM OR ti:LLMs)"#),
    tag!("agents", "LLM agents, tool use", r#"((ti:agent OR ti:agents OR ti:agentic) AND (abs:"language model" OR abs:LLM))"#),
    tag!("reasoning", "LLM reasoning, chain of thought", r#"(ti:reasoning AND (abs:"language model" OR abs:LLM))"#),
    tag!("rl", "reinforcement learning", r#"(ti:"reinforcement learning")"#),
    tag!("rlhf", "RLHF, preference optimisation, alignment", r#"(ti:RLHF OR ti:"preference optimization" OR ti:"reward model" OR (ti:alignment AND abs:"language model"))"#),
    tag!("diffusion", "diffusion / score-based generative models", r#"(ti:"diffusion model" OR ti:"diffusion models" OR ti:"denoising diffusion" OR ti:"score-based")"#),
    tag!("transformers", "transformer architectures, attention", r#"(ti:transformer OR ti:transformers OR ti:"attention mechanism")"#),
    tag!("interpretability", "mechanistic interpretability, sparse autoencoders", r#"(ti:interpretability OR ti:"sparse autoencoders" OR ti:"mechanistic interpretability")"#),
    tag!("scaling-laws", "scaling laws", r#"(ti:"scaling law" OR ti:"scaling laws")"#),
    tag!("rag", "retrieval-augmented generation", r#"(ti:"retrieval-augmented" OR ti:RAG OR ti:"retrieval augmented")"#),
    tag!("benchmark", "benchmarks and datasets", r#"(ti:benchmark OR ti:benchmarks)"#),
    tag!("survey", "surveys and literature reviews", r#"(ti:survey OR ti:"literature review" OR ti:"systematic review")"#),
    tag!("gnn", "graph neural networks", r#"(ti:"graph neural network" OR ti:"graph neural networks")"#),
    tag!("causal", "causal inference and discovery", r#"(ti:"causal inference" OR ti:"causal discovery" OR ti:"causal effect")"#),
    tag!("bayesian", "Bayesian inference and methods", r#"(ti:Bayesian)"#),
    tag!("optimization", "optimisation theory and algorithms", r#"(ti:"convex optimization" OR ti:"stochastic optimization" OR ti:"nonconvex optimization" OR ti:"gradient descent")"#),
    tag!("theorem-proving", "(automated/neural) theorem proving", r#"(ti:"theorem proving" OR abs:"automated theorem proving" OR abs:"neural theorem proving")"#),
    tag!("formal-math", "Lean, Isabelle, Coq, formal mathematics", r#"(abs:"Lean 4" OR abs:Mathlib OR abs:"Isabelle/HOL" OR abs:"proof assistant" OR abs:"formal mathematics")"#),
    tag!("autoformalization", "autoformalization", r#"(ti:autoformalization OR abs:autoformalization OR abs:autoformalisation OR abs:"auto-formalization")"#),
    tag!("math-reasoning", "LLM mathematical reasoning", r#"((ti:"mathematical reasoning" OR abs:"mathematical reasoning" OR abs:"math reasoning") AND (abs:"language model" OR abs:LLM))"#),
    tag!("combinatorics", "combinatorics", r#"(ti:combinatorics OR ti:combinatorial)"#),
    tag!("graph-theory", "graph theory", r#"(ti:"graph theory" OR ti:"graph theoretic" OR ti:"graph-theoretic")"#),
    tag!("number-theory", "number theory", r#"(ti:"number theory" OR ti:"number-theoretic")"#),
    tag!("ramsey", "Ramsey theory and numbers", r#"(ti:Ramsey OR abs:"Ramsey number" OR abs:"Ramsey numbers" OR abs:"Ramsey theory")"#),
    tag!("extremal", "extremal combinatorics/graph theory, Turan", r#"(ti:"extremal combinatorics" OR ti:"extremal graph" OR abs:"extremal graph theory" OR ti:Turan)"#),
    tag!("additive-combinatorics", "additive combinatorics, Freiman", r#"(ti:"additive combinatorics" OR ti:Freiman OR abs:Freiman)"#),
    tag!("probabilistic-method", "probabilistic method, Lovasz local lemma", r#"(ti:"probabilistic method" OR abs:"probabilistic method" OR abs:"Lovasz local lemma")"#),
    tag!("erdos", "Erdos problems", r#"(ti:"Erdos problems" OR abs:"Erdos problems" OR ti:"Erdos problem" OR abs:"Erdos problem")"#),
    tag!("pde", "partial differential equations", r#"(ti:"partial differential equation" OR ti:"partial differential equations" OR ti:PDE OR ti:PDEs)"#),
    tag!("fluids", "turbulence, Navier-Stokes", r#"(ti:turbulence OR ti:"Navier-Stokes")"#),
    tag!("quantum-computing", "quantum algorithms and circuits", r#"(ti:"quantum computing" OR ti:"quantum algorithm" OR ti:"quantum algorithms" OR ti:"quantum circuit")"#),
    tag!("quantum-error-correction", "quantum error correction", r#"(ti:"quantum error correction" OR ti:"quantum error-correcting" OR ti:"surface code")"#),
    tag!("cryptography", "cryptography, post-quantum, zero-knowledge", r#"(ti:cryptography OR ti:"post-quantum" OR ti:"zero-knowledge" OR ti:"homomorphic encryption")"#),
    tag!("genomics", "genomics and sequencing", r#"(ti:genomics OR ti:genome OR ti:"single-cell")"#),
    tag!("protein", "protein structure and design", r#"(ti:"protein structure" OR ti:"protein design" OR ti:AlphaFold OR ti:"protein folding")"#),
    tag!("climate", "climate science and modelling", r#"(ti:climate)"#),
    tag!("cosmology", "cosmology, dark matter, dark energy", r#"(ti:cosmology OR ti:"dark matter" OR ti:"dark energy")"#),
    tag!("gravitational-waves", "gravitational waves, black holes", r#"(ti:"gravitational wave" OR ti:"gravitational waves" OR ti:"black hole" OR ti:"black holes")"#),
    tag!("superconductivity", "superconductors, topological matter", r#"(ti:superconductivity OR ti:superconductor OR ti:"topological insulator" OR ti:"topological phase")"#),
];

static TAG_ALIASES: &[(&str, &str)] = &[
    ("llms", "llm"),
    ("agent", "agents"),
    ("agentic", "agents"),
    ("cot", "reasoning"),
    ("reinforcement-learning", "rl"),
    ("alignment", "rlhf"),
    ("score-based", "diffusion"),
    ("transformer", "transformers"),
    ("sae", "interpretability"),
    ("scaling", "scaling-laws"),
    ("retrieval", "rag"),
    ("benchmarks", "benchmark"),
    ("dataset", "benchmark"),
    ("review", "survey"),
    ("gnns", "gnn"),
    ("causality", "causal"),
    ("convex", "optimization"),
    ("atp", "theorem-proving"),
    ("lean", "formal-math"),
    ("formal", "formal-math"),
    ("isabelle", "formal-math"),
    ("coq", "formal-math"),
    ("autoformal", "autoformalization"),
    ("math-llm", "math-reasoning"),
    ("comb", "combinatorics"),
    ("combinatorial", "combinatorics"),
    ("graphs", "graph-theory"),
    ("graph", "graph-theory"),
    ("nt", "number-theory"),
    ("numbertheory", "number-theory"),
    ("additive", "additive-combinatorics"),
    ("addcomb", "additive-combinatorics"),
    ("pdes", "pde"),
    ("turbulence", "fluids"),
    ("quantum", "quantum-computing"),
    ("qec", "quantum-error-correction"),
    ("crypto", "cryptography"),
    ("genome", "genomics"),
    ("alphafold", "protein"),
    ("dark-matter", "cosmology"),
    ("gw", "gravitational-waves"),
    ("black-holes", "gravitational-waves"),
    ("topological", "superconductivity"),
];

fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.trim().chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Equal, or (both >= 5 chars and) edit distance <= 1.
fn near(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len() < 5 || b.len() < 5 || a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (s, l) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    let i = s.iter().zip(l.iter()).take_while(|(x, y)| x == y).count();
    if s.len() == l.len() {
        s[i + 1..] == l[i + 1..]
    } else {
        s[i..] == l[i + 1..]
    }
}

/// Fuzzy/alias resolution ("nt" -> number-theory, "ineq" -> inequalities).
pub fn tag(name: &str) -> Option<&'static Tag> {
    let n = slug(name);
    if n.is_empty() {
        return None;
    }
    let by = |t: &str| TAGS.iter().find(|x| x.name == t);
    if let Some(t) = by(&n) {
        return Some(t);
    }
    if let Some((_, t)) = TAG_ALIASES.iter().find(|(a, _)| *a == n) {
        return by(t);
    }
    let sq = squash(&n);
    if let Some(t) = TAGS.iter().find(|t| squash(t.name) == sq) {
        return Some(t);
    }
    if n.len() >= 3 {
        if let Some(t) = TAGS.iter().find(|t| t.name.starts_with(&n)) {
            return Some(t);
        }
    }
    TAGS.iter()
        .find(|t| near(&n, t.name) || near(&sq, &squash(t.name)))
        .or_else(|| TAG_ALIASES.iter().find(|(a, _)| near(&n, a)).and_then(|(_, t)| by(t)))
}

pub fn list_tags() -> String {
    let mut out = String::new();
    for t in TAGS {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(t.name);
        out.push_str("  ");
        out.push_str(t.desc);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balanced(q: &str) -> bool {
        let (mut depth, mut inq) = (0i32, false);
        for c in q.chars() {
            match c {
                '"' => inq = !inq,
                '(' if !inq => depth += 1,
                ')' if !inq => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth == 0 && !inq
    }

    #[test]
    fn tag_names_unique_and_queries_wellformed() {
        let mut names: Vec<_> = TAGS.iter().map(|t| t.name).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n);
        for t in TAGS {
            assert!(balanced(t.query), "{}", t.name);
            assert!(t.query.starts_with('(') && t.query.ends_with(')'), "{}", t.name);
            assert!(t.query.is_ascii(), "{}", t.name);
            assert!(!t.desc.is_empty());
            assert!(t.query.contains("ti:") || t.query.contains("abs:"));
        }
        for (a, t) in TAG_ALIASES {
            assert!(TAGS.iter().any(|x| x.name == *t), "alias {a} -> {t}");
            assert!(TAGS.iter().all(|x| x.name != *a), "alias shadows tag {a}");
        }
    }

    #[test]
    fn tag_lookup() {
        let n = |s: &str| tag(s).map(|t| t.name);
        assert_eq!(n("nt"), Some("number-theory"));
        assert_eq!(n("LLMs"), Some("llm"));
        assert_eq!(n("number theory"), Some("number-theory"));
        assert_eq!(n("numbertheory"), Some("number-theory"));
        assert_eq!(n("Scaling_Laws"), Some("scaling-laws"));
        assert_eq!(n("combinatoric"), Some("combinatorics"));
        assert_eq!(n("combinatorcs"), Some("combinatorics"));
        assert_eq!(n("lean"), Some("formal-math"));
        assert_eq!(n("ramsy"), Some("ramsey"));
        assert_eq!(n(""), None);
        assert_eq!(n("   "), None);
        assert_eq!(n("zzzzzz"), None);
        assert_eq!(n("日本語"), None);
        assert_eq!(n("--"), None);
    }
}
