use super::*;

#[test]
fn a_file_uri_escapes_what_a_path_segment_cannot_hold() {
    assert_eq!(file_uri(std::path::Path::new("/home/u/OneDrive/plain.jpg")), "file:///home/u/OneDrive/plain.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/with space.jpg")), "file:///h/with%20space.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/фото.jpg")), "file:///h/%D1%84%D0%BE%D1%82%D0%BE.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/sym (1)+&,;=@:!$'*.jpg")), "file:///h/sym%20(1)+&,;=@:!$'*.jpg");
    assert_eq!(file_uri(std::path::Path::new("/h/100%.jpg")), "file:///h/100%25.jpg");
}
