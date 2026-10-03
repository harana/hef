use super::*;

fn strings(values: &[Option<&str>]) -> StringColumn {
    values.iter().copied().collect()
}

#[test]
fn round_trips_present_and_absent_rows() {
    let values = [Some("alpha"), None, Some(""), Some("omega")];
    let column = strings(&values);
    assert_eq!(column.len(), 4);
    assert_eq!(column.iter().collect::<Vec<_>>(), values.to_vec());
}

#[test]
fn a_present_empty_string_is_not_an_absent_row() {
    let column = strings(&[Some(""), None]);
    assert_eq!(column.get(0), Some(Some("")));
    assert_eq!(column.get(1), Some(None));
    assert_eq!(column.null_count(), 1);
}

#[test]
fn get_past_the_last_row_is_none() {
    assert_eq!(strings(&[Some("only")]).get(1), None);
}

#[test]
fn absent_rows_contribute_no_text() {
    let column = strings(&[Some("ab"), None, Some("cde")]);
    assert_eq!(column.text_len(), 5);
    assert_eq!(column.iter_present().collect::<Vec<_>>(), vec!["ab", "cde"]);
}

#[test]
fn slicing_keeps_only_its_own_rows_and_text() {
    let column = strings(&[Some("aa"), None, Some("bb"), Some("cc")]);
    let sliced = column.slice(1, 3);
    assert_eq!(sliced.iter().collect::<Vec<_>>(), vec![None, Some("bb")]);
    assert_eq!(sliced.text_len(), 2);
    assert_eq!(sliced, strings(&[None, Some("bb")]));
}

#[test]
fn slicing_clamps_past_the_end_and_empties_an_inverted_range() {
    let column = strings(&[Some("aa"), Some("bb")]);
    assert_eq!(column.slice(1, 99).iter().collect::<Vec<_>>(), vec![Some("bb")]);
    assert!(column.slice(2, 1).is_empty());
}

#[test]
fn appending_concatenates_rows() {
    let mut column = strings(&[Some("aa"), None]);
    column.append(&strings(&[Some("bb")]));
    assert_eq!(column, strings(&[Some("aa"), None, Some("bb")]));
}

#[test]
fn equality_ignores_how_the_column_was_built() {
    let mut built = StringColumn::with_capacity(0, 0);
    built.push(Some("aa"));
    built.push(None);
    built.push(Some("bb"));
    assert_eq!(built, strings(&[Some("aa"), None, Some("bb")]));
}

#[test]
fn multi_byte_text_slices_on_character_boundaries() {
    let column = strings(&[Some("héllo"), Some("wörld")]);
    assert_eq!(column.get(0), Some(Some("héllo")));
    assert_eq!(column.get(1), Some(Some("wörld")));
}

#[test]
fn debug_reads_as_the_values() {
    assert_eq!(format!("{:?}", strings(&[Some("a"), None])), r#"[Some("a"), None]"#);
}
