fn add(a: i32, b: i32) -> i32 { a + b }

fn main() { println!("{}", add(2, 2)); }

#[test]
fn test_add() { assert_eq!(add(2, 2), 4); }
