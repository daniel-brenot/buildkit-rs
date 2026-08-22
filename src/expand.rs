use std::collections::HashMap;

/// Substitute `$VAR` / `${VAR}` using the provided map.
pub fn expand(input: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            if chars[i + 1] == '{' {
                if let Some(end) = chars[i + 2..].iter().position(|c| *c == '}') {
                    let key: String = chars[i + 2..i + 2 + end].iter().collect();
                    if let Some(val) = vars.get(&key) {
                        out.push_str(val);
                    }
                    i += 3 + end;
                    continue;
                }
            } else if chars[i + 1].is_ascii_alphanumeric() || chars[i + 1] == '_' {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let key: String = chars[i + 1..j].iter().collect();
                if let Some(val) = vars.get(&key) {
                    out.push_str(val);
                }
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn expand_vec(args: &[String], vars: &HashMap<String, String>) -> Vec<String> {
    args.iter().map(|a| expand(a, vars)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_vars() {
        let mut vars = HashMap::new();
        vars.insert("NAME".into(), "world".into());
        assert_eq!(expand("hello $NAME", &vars), "hello world");
        assert_eq!(expand("hello ${NAME}!", &vars), "hello world!");
        assert_eq!(expand("hello $MISSING", &vars), "hello ");
    }
}
