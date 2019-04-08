// Copyright 2019 Facebook, Inc.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

mod cursor;
mod link;
mod store;

use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap},
    sync::Arc,
};

use crypto::{digest::Digest, sha1::Sha1};
use failure::{bail, format_err, Fallible};
use once_cell::sync::OnceCell;

use types::{Node, PathComponent, RepoPath, RepoPathBuf};

use self::cursor::{Cursor, Step};
use self::link::{Durable, DurableEntry, Ephemeral, Leaf, Link};
use self::store::Store;
use crate::{FileMetadata, Manifest};

/// The Tree implementation of a Manifest dedicates an inner node for each directory in the
/// repository and a leaf for each file.
pub struct Tree<S> {
    store: S,
    // TODO: root can't be a Leaf
    root: Link,
}

impl<S> Tree<S> {
    /// Instantiates a tree manifest that was stored with the specificed `Node`
    pub fn durable(store: S, node: Node) -> Self {
        Tree {
            store,
            root: Link::durable(node),
        }
    }

    /// Instantiates a new tree manifest with no history
    pub fn ephemeral(store: S) -> Self {
        Tree {
            store,
            root: Link::Ephemeral(BTreeMap::new()),
        }
    }

    /// Returns an iterator over all the files that are present in the tree.
    pub fn files<'a>(&'a self) -> Files<'a, S> {
        Files {
            cursor: self.root_cursor(),
        }
    }

    fn root_cursor<'a>(&'a self) -> Cursor<'a, S> {
        Cursor::new(&self.store, RepoPathBuf::new(), &self.root)
    }
}

impl<S: Store> Manifest for Tree<S> {
    fn get(&self, path: &RepoPath) -> Fallible<Option<&FileMetadata>> {
        match self.get_link(path)? {
            None => Ok(None),
            Some(link) => {
                if let Leaf(file_metadata) = link {
                    Ok(Some(file_metadata))
                } else {
                    Err(format_err!("Encountered directory where file was expected"))
                }
            }
        }
    }

    fn insert(&mut self, path: RepoPathBuf, file_metadata: FileMetadata) -> Fallible<()> {
        let (parent, last_component) = match path.split_last_component() {
            Some(v) => v,
            None => bail!("Cannot insert file metadata for repository root"),
        };
        let mut cursor = &mut self.root;
        for (cursor_parent, component) in parent.parents().zip(parent.components()) {
            // TODO: only convert to ephemeral when a mutation takes place.
            cursor = cursor
                .mut_ephemeral_links(&self.store, cursor_parent)?
                .entry(component.to_owned())
                .or_insert_with(|| Ephemeral(BTreeMap::new()));
        }
        match cursor
            .mut_ephemeral_links(&self.store, parent)?
            .entry(last_component.to_owned())
        {
            Entry::Vacant(entry) => {
                entry.insert(Link::Leaf(file_metadata));
            }
            Entry::Occupied(mut entry) => {
                if let Leaf(ref mut store_ref) = entry.get_mut() {
                    *store_ref = file_metadata;
                } else {
                    bail!("Encountered directory where file was expected");
                }
            }
        }
        Ok(())
    }

    // TODO: return Fallible<Option<FileMetadata>>
    fn remove(&mut self, path: &RepoPath) -> Fallible<()> {
        // The return value lets us know if there are no more files in the subtree and we should be
        // removing it.
        fn do_remove<'a, S, I>(store: &S, cursor: &mut Link, iter: &mut I) -> Fallible<bool>
        where
            S: Store,
            I: Iterator<Item = (&'a RepoPath, &'a PathComponent)>,
        {
            match iter.next() {
                None => {
                    if let Leaf(_) = cursor {
                        // We reached the file that we want to remove.
                        Ok(true)
                    } else {
                        // TODO: add directory to message.
                        // It turns out that the path we were asked to remove is a directory.
                        Err(format_err!("Asked to remove a directory."))
                    }
                }
                Some((parent, component)) => {
                    // TODO: only convert to ephemeral if a removal took place
                    // We are navigating the tree down following parent directories
                    let ephemeral_links = cursor.mut_ephemeral_links(store, parent)?;
                    // When there is no `component` subtree we behave like the file was removed.
                    if let Some(link) = ephemeral_links.get_mut(component) {
                        if do_remove(store, link, iter)? {
                            // There are no files in the component subtree so we remove it.
                            ephemeral_links.remove(component);
                        }
                    }
                    Ok(ephemeral_links.is_empty())
                }
            }
        }
        do_remove(
            &self.store,
            &mut self.root,
            &mut path.parents().zip(path.components()),
        )?;
        Ok(())
    }

    // NOTE: incomplete implementation, currently using dummy values for parents in hash
    // computation. Works fine for testing but hashes don't match other implementations.
    fn flush(&mut self) -> Fallible<Node> {
        fn compute_node<C: AsRef<[u8]>>(p1: &Node, p2: &Node, content: C) -> Node {
            let mut hasher = Sha1::new();
            if p1 < p2 {
                hasher.input(p1.as_ref());
                hasher.input(p2.as_ref());
            } else {
                hasher.input(p2.as_ref());
                hasher.input(p1.as_ref());
            }
            hasher.input(content.as_ref());
            let mut buf = [0u8; Node::len()];
            hasher.result(&mut buf);
            (&buf).into()
        }
        fn do_flush<'a, 'b, 'c, S: Store>(
            store: &'a mut S,
            pathbuf: &'b mut RepoPathBuf,
            cursor: &'c mut Link,
        ) -> Fallible<(&'c Node, store::Flag)> {
            loop {
                match cursor {
                    Leaf(file_metadata) => {
                        return Ok((
                            &file_metadata.node,
                            store::Flag::File(file_metadata.file_type.clone()),
                        ));
                    }
                    Durable(entry) => return Ok((&entry.node, store::Flag::Directory)),
                    Ephemeral(links) => {
                        let iter = links.iter_mut().map(|(component, link)| {
                            pathbuf.push(component.as_path_component());
                            let (node, flag) = do_flush(store, pathbuf, link)?;
                            pathbuf.pop();
                            Ok(store::Element::new(
                                component.to_owned(),
                                node.clone(),
                                flag,
                            ))
                        });
                        let entry = store::Entry::from_elements(iter)?;
                        // TODO: use actual parent node values
                        let node = compute_node(Node::null_id(), Node::null_id(), &entry);

                        // TODO: insert the linknode as part of the store.insert
                        store.insert(pathbuf.clone(), node.clone(), entry)?;

                        let cell = OnceCell::new();
                        // TODO: remove clone
                        cell.set(Ok(links.clone())).unwrap();

                        let durable_entry = DurableEntry { node, links: cell };
                        *cursor = Durable(Arc::new(durable_entry));
                    }
                }
            }
        }
        let mut path = RepoPathBuf::new();
        let (node, _) = do_flush(&mut self.store, &mut path, &mut self.root)?;
        Ok(node.clone())
    }
}

impl<S: Store> Tree<S> {
    fn get_link(&self, path: &RepoPath) -> Fallible<Option<&Link>> {
        let mut cursor = &self.root;
        for (parent, component) in path.parents().zip(path.components()) {
            let child = match cursor {
                Leaf(_) => bail!("Encountered file where a directory was expected."),
                Ephemeral(links) => links.get(component),
                Durable(ref entry) => {
                    let links = entry.get_links(&self.store, parent)?;
                    links.get(component)
                }
            };
            match child {
                None => return Ok(None),
                Some(link) => cursor = link,
            }
        }
        Ok(Some(cursor))
    }
}

pub struct Files<'a, S> {
    cursor: Cursor<'a, S>,
}

impl<'a, S: Store> Iterator for Files<'a, S> {
    type Item = Fallible<(RepoPathBuf, FileMetadata)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.cursor.step() {
                Step::Success => {
                    if let Leaf(file_metadata) = self.cursor.link() {
                        return Some(Ok((self.cursor.path().to_owned(), *file_metadata)));
                    }
                }
                Step::Err(error) => return Some(Err(error)),
                Step::End => return None,
            }
        }
    }
}

/// Returns an iterator over all the differences between two [`Tree`]s. Keeping in mind that
/// manifests operate over files, the difference space is limited to three cases described by
/// [`DiffType`]:
///  * a file may be present only in the left tree manifest
///  * a file may be present only in the right tree manifest
///  * a file may have different file_metadata between the two tree manifests
///
/// For the case where we have the the file "foo" in the `left` tree manifest and we have the "foo"
/// directory in the `right` tree manifest, the differences returned will be:
///  1. DiffEntry("foo", LeftOnly(_))
///  2. DiffEntry(file, RightOnly(_)) for all `file`s under the "foo" directory
pub fn diff<'a, S: Store>(left: &'a Tree<S>, right: &'a Tree<S>) -> Diff<'a, S> {
    Diff {
        left: left.root_cursor(),
        step_left: false,
        right: right.root_cursor(),
        step_right: false,
    }
}

/// An iterator over the differences between two tree manifests.
/// See [`diff()`].
pub struct Diff<'a, S> {
    left: Cursor<'a, S>,
    step_left: bool,
    right: Cursor<'a, S>,
    step_right: bool,
}

/// Represents a file that is different between two tree manifests.
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct DiffEntry {
    path: RepoPathBuf,
    diff_type: DiffType,
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum DiffType {
    LeftOnly(FileMetadata),
    RightOnly(FileMetadata),
    Changed(FileMetadata, FileMetadata),
}

impl DiffEntry {
    fn new(path: RepoPathBuf, diff_type: DiffType) -> Self {
        DiffEntry { path, diff_type }
    }
}

impl<'a, S: Store> Iterator for Diff<'a, S> {
    type Item = Fallible<DiffEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        // This is the standard algorithm for returning the differences in two lists but adjusted
        // to have the iterator interface and to evaluate the tree lazily.
        fn diff_entry(path: &RepoPath, diff_type: DiffType) -> Option<Fallible<DiffEntry>> {
            Some(Ok(DiffEntry::new(path.to_owned(), diff_type)))
        }
        fn compare<'a, S>(left: &Cursor<'a, S>, right: &Cursor<'a, S>) -> Option<Ordering> {
            // TODO: cache ordering state so we compare last components at most
            match (left.finished(), right.finished()) {
                (true, true) => None,
                (false, true) => Some(Ordering::Less),
                (true, false) => Some(Ordering::Greater),
                (false, false) => Some(left.path().cmp(right.path())),
            }
        }
        loop {
            if self.step_left {
                if let Step::Err(error) = self.left.step() {
                    return Some(Err(error));
                }
                self.step_left = false;
            }
            if self.step_right {
                if let Step::Err(error) = self.right.step() {
                    return Some(Err(error));
                }
                self.step_right = false;
            }
            match compare(&self.left, &self.right) {
                None => return None,
                Some(Ordering::Less) => {
                    self.step_left = true;
                    if let Leaf(file_metadata) = self.left.link() {
                        return diff_entry(self.left.path(), DiffType::LeftOnly(*file_metadata));
                    }
                }
                Some(Ordering::Greater) => {
                    self.step_right = true;
                    if let Leaf(file_metadata) = self.right.link() {
                        return diff_entry(self.right.path(), DiffType::RightOnly(*file_metadata));
                    }
                }
                Some(Ordering::Equal) => {
                    self.step_left = true;
                    self.step_right = true;
                    match (self.left.link(), self.right.link()) {
                        (Leaf(left_metadata), Leaf(right_metadata)) => {
                            if left_metadata != right_metadata {
                                return diff_entry(
                                    self.left.path(),
                                    DiffType::Changed(*left_metadata, *right_metadata),
                                );
                            }
                        }
                        (Leaf(file_metadata), _) => {
                            return diff_entry(self.left.path(), DiffType::LeftOnly(*file_metadata));
                        }
                        (_, Leaf(file_metadata)) => {
                            return diff_entry(
                                self.right.path(),
                                DiffType::RightOnly(*file_metadata),
                            );
                        }
                        (Durable(left_entry), Durable(right_entry)) => {
                            if left_entry.node == right_entry.node {
                                self.left.skip_subtree();
                                self.right.skip_subtree();
                            }
                        }
                        // All other cases are two directories that we have to iterate over
                        _ => (),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use types::testutil::*;
    use types::PathComponentBuf;

    use self::store::TestStore;
    use crate::FileType;

    fn meta(node: u8) -> FileMetadata {
        FileMetadata::regular(Node::from_u8(node))
    }
    fn store_element(path: &str, node: u8, flag: store::Flag) -> Fallible<store::Element> {
        Ok(store::Element::new(
            path_component_buf(path),
            Node::from_u8(node),
            flag,
        ))
    }

    #[test]
    fn test_insert() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("foo/bar"), meta(10)).unwrap();
        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), Some(&meta(10)));
        assert_eq!(tree.get(repo_path("baz")).unwrap(), None);

        tree.insert(repo_path_buf("baz"), meta(20)).unwrap();
        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), Some(&meta(10)));
        assert_eq!(tree.get(repo_path("baz")).unwrap(), Some(&meta(20)));

        tree.insert(repo_path_buf("foo/bat"), meta(30)).unwrap();
        assert_eq!(tree.get(repo_path("foo/bat")).unwrap(), Some(&meta(30)));
        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), Some(&meta(10)));
        assert_eq!(tree.get(repo_path("baz")).unwrap(), Some(&meta(20)));
    }

    #[test]
    fn test_durable_link() {
        let mut store = TestStore::new();
        let root_entry = store::Entry::from_elements(vec![
            store_element("foo", 10, store::Flag::Directory),
            store_element("baz", 20, store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(RepoPathBuf::new(), Node::from_u8(1), root_entry)
            .unwrap();
        let foo_entry = store::Entry::from_elements(vec![store_element(
            "bar",
            11,
            store::Flag::File(FileType::Regular),
        )])
        .unwrap();
        store
            .insert(repo_path_buf("foo"), Node::from_u8(10), foo_entry)
            .unwrap();
        let mut tree = Tree::durable(store, Node::from_u8(1));

        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), Some(&meta(11)));
        assert_eq!(tree.get(repo_path("baz")).unwrap(), Some(&meta(20)));

        tree.insert(repo_path_buf("foo/bat"), meta(12)).unwrap();
        assert_eq!(tree.get(repo_path("foo/bat")).unwrap(), Some(&meta(12)));
        assert_eq!(tree.get(repo_path("foo/bar")).unwrap(), Some(&meta(11)));
        assert_eq!(tree.get(repo_path("baz")).unwrap(), Some(&meta(20)));
    }

    #[test]
    fn test_insert_into_directory() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("foo/bar/baz"), meta(10)).unwrap();
        assert!(tree.insert(repo_path_buf("foo/bar"), meta(20)).is_err());
        assert!(tree.insert(repo_path_buf("foo"), meta(30)).is_err());
    }

    #[test]
    fn test_insert_with_file_parent() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("foo"), meta(10)).unwrap();
        assert!(tree.insert(repo_path_buf("foo/bar"), meta(20)).is_err());
        assert!(tree.insert(repo_path_buf("foo/bar/baz"), meta(30)).is_err());
    }

    #[test]
    fn test_get_from_directory() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("foo/bar/baz"), meta(10)).unwrap();
        assert!(tree.get(repo_path("foo/bar")).is_err());
        assert!(tree.get(repo_path("foo")).is_err());
    }

    #[test]
    fn test_get_with_file_parent() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("foo"), meta(10)).unwrap();
        assert!(tree.get(repo_path("foo/bar")).is_err());
        assert!(tree.get(repo_path("foo/bar/baz")).is_err());
    }

    #[test]
    fn test_remove_from_ephemeral() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), meta(10)).unwrap();
        tree.insert(repo_path_buf("a1/b2"), meta(20)).unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();

        assert!(tree.remove(repo_path("a1")).is_err());
        assert!(tree.remove(repo_path("a1/b1")).is_err());
        assert!(tree.remove(repo_path("a1/b1/c1/d1/e1")).is_err());
        tree.remove(repo_path("a1/b1/c1/d1")).unwrap();
        tree.remove(repo_path("a3")).unwrap(); // does nothing
        tree.remove(repo_path("a1/b3")).unwrap(); // does nothing
        tree.remove(repo_path("a1/b1/c1/d2")).unwrap(); // does nothing
        tree.remove(repo_path("a1/b1/c1/d1/e1")).unwrap(); // does nothing
        assert!(tree.remove(RepoPath::empty()).is_err());
        assert_eq!(tree.get(repo_path("a1/b1/c1/d1")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b1/c1")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b2")).unwrap(), Some(&meta(20)));
        tree.remove(repo_path("a1/b2")).unwrap();
        assert_eq!(tree.get_link(repo_path("a1")).unwrap(), None);

        assert_eq!(tree.get(repo_path("a2/b2/c2")).unwrap(), Some(&meta(30)));
        tree.remove(repo_path("a2/b2/c2")).unwrap();
        assert_eq!(tree.get(repo_path("a2")).unwrap(), None);

        assert!(tree.get_link(RepoPath::empty()).unwrap().is_some());
    }

    #[test]
    fn test_remove_from_durable() {
        let mut store = TestStore::new();
        let root_entry = store::Entry::from_elements(vec![
            store_element("a1", 10, store::Flag::Directory),
            store_element("a2", 20, store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(RepoPathBuf::new(), Node::from_u8(1), root_entry)
            .unwrap();
        let a1_entry = store::Entry::from_elements(vec![
            store_element("b1", 11, store::Flag::File(FileType::Regular)),
            store_element("b2", 12, store::Flag::File(FileType::Regular)),
        ])
        .unwrap();
        store
            .insert(repo_path_buf("a1"), Node::from_u8(10), a1_entry)
            .unwrap();
        let mut tree = Tree::durable(store, Node::from_u8(1));

        assert!(tree.remove(repo_path("a1")).is_err());
        tree.remove(repo_path("a1/b1")).unwrap();
        assert_eq!(tree.get(repo_path("a1/b1")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1/b2")).unwrap(), Some(&meta(12)));
        tree.remove(repo_path("a1/b2")).unwrap();
        assert_eq!(tree.get(repo_path("a1/b2")).unwrap(), None);
        assert_eq!(tree.get(repo_path("a1")).unwrap(), None);
        assert_eq!(tree.get_link(repo_path("a1")).unwrap(), None);

        assert_eq!(tree.get(repo_path("a2")).unwrap(), Some(&meta(20)));
        tree.remove(repo_path("a2")).unwrap();
        assert_eq!(tree.get(repo_path("a2")).unwrap(), None);

        assert!(tree.get_link(RepoPath::empty()).unwrap().is_some());
    }

    #[test]
    fn test_flush() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), meta(10)).unwrap();
        tree.insert(repo_path_buf("a1/b2"), meta(20)).unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();

        let node = tree.flush().unwrap();
        let store = tree.store;

        let tree = Tree::durable(store, node);
        assert_eq!(tree.get(repo_path("a1/b1/c1/d1")).unwrap(), Some(&meta(10)));
        assert_eq!(tree.get(repo_path("a1/b2")).unwrap(), Some(&meta(20)));
        assert_eq!(tree.get(repo_path("a2/b2/c2")).unwrap(), Some(&meta(30)));
        assert_eq!(tree.get(repo_path("a2/b1")).unwrap(), None);
    }

    #[test]
    fn test_cursor_skip_on_root() {
        let tree = Tree::ephemeral(TestStore::new());
        let mut cursor = tree.root_cursor();
        cursor.skip_subtree();
        match cursor.step() {
            Step::Success => panic!("should have reached the end of the tree"),
            Step::End => (), // success
            Step::Err(error) => panic!(error),
        }
    }
    #[test]
    fn test_cursor_skip() {
        fn step<'a, S: Store>(cursor: &mut Cursor<'a, S>) {
            match cursor.step() {
                Step::Success => (),
                Step::End => panic!("reached the end too soon"),
                Step::Err(error) => panic!(error),
            }
        }
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("a1"), meta(10)).unwrap();
        tree.insert(repo_path_buf("a2/b2"), meta(20)).unwrap();
        tree.insert(repo_path_buf("a3"), meta(30)).unwrap();

        let mut cursor = tree.root_cursor();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::empty());
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a1").unwrap());
        // Skip leaf
        cursor.skip_subtree();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a2").unwrap());
        // Skip directory
        cursor.skip_subtree();
        step(&mut cursor);
        assert_eq!(cursor.path(), RepoPath::from_str("a3").unwrap());
        // Skip on the element before State::End
        cursor.skip_subtree();
        match cursor.step() {
            Step::Success => panic!("should have reached the end of the tree"),
            Step::End => (), // success
            Step::Err(error) => panic!(error),
        }
    }

    #[test]
    fn test_files_empty() {
        let tree = Tree::ephemeral(TestStore::new());
        assert!(tree.files().next().is_none());
    }

    #[test]
    fn test_files_ephemeral() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), meta(10)).unwrap();
        tree.insert(repo_path_buf("a1/b2"), meta(20)).unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();

        let mut files = tree.files();
        assert_eq!(
            files.next().unwrap().unwrap(),
            (repo_path_buf("a1/b1/c1/d1"), meta(10))
        );
        assert_eq!(
            files.next().unwrap().unwrap(),
            (repo_path_buf("a1/b2"), meta(20))
        );
        assert_eq!(
            files.next().unwrap().unwrap(),
            (repo_path_buf("a2/b2/c2"), meta(30))
        );
        assert!(files.next().is_none());
    }

    #[test]
    fn test_files_durable() {
        let mut tree = Tree::ephemeral(TestStore::new());
        tree.insert(repo_path_buf("a1/b1/c1/d1"), meta(10)).unwrap();
        tree.insert(repo_path_buf("a1/b2"), meta(20)).unwrap();
        tree.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();
        let node = tree.flush().unwrap();
        let store = tree.store;
        let tree = Tree::durable(store, node);

        let mut files = tree.files();
        assert_eq!(
            files.next().unwrap().unwrap(),
            (repo_path_buf("a1/b1/c1/d1"), meta(10))
        );
        assert_eq!(
            files.next().unwrap().unwrap(),
            (repo_path_buf("a1/b2"), meta(20))
        );
        assert_eq!(
            files.next().unwrap().unwrap(),
            (repo_path_buf("a2/b2/c2"), meta(30))
        );
        assert!(files.next().is_none());
    }

    #[test]
    fn test_files_finish_on_error_when_collecting_to_vec() {
        let tree = Tree::durable(TestStore::new(), Node::from_u8(1));
        let file_results = tree.files().collect::<Vec<_>>();
        assert_eq!(file_results.len(), 1);
        assert!(file_results[0].is_err());

        let files_result = tree.files().collect::<Result<Vec<_>, _>>();
        assert!(files_result.is_err());
    }

    #[test]
    fn test_diff_generic() {
        let mut left = Tree::ephemeral(TestStore::new());
        left.insert(repo_path_buf("a1/b1/c1/d1"), meta(10)).unwrap();
        left.insert(repo_path_buf("a1/b2"), meta(20)).unwrap();
        left.insert(repo_path_buf("a3/b1"), meta(40)).unwrap();

        let mut right = Tree::ephemeral(TestStore::new());
        right.insert(repo_path_buf("a1/b2"), meta(40)).unwrap();
        right.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();
        right.insert(repo_path_buf("a3/b1"), meta(40)).unwrap();

        assert_eq!(
            diff(&left, &right).collect::<Fallible<Vec<_>>>().unwrap(),
            vec!(
                DiffEntry::new(repo_path_buf("a1/b1/c1/d1"), DiffType::LeftOnly(meta(10))),
                DiffEntry::new(
                    repo_path_buf("a1/b2"),
                    DiffType::Changed(meta(20), meta(40))
                ),
                DiffEntry::new(repo_path_buf("a2/b2/c2"), DiffType::RightOnly(meta(30))),
            )
        );

        left.flush().unwrap();
        right.flush().unwrap();

        assert_eq!(
            diff(&left, &right).collect::<Fallible<Vec<_>>>().unwrap(),
            vec!(
                DiffEntry::new(repo_path_buf("a1/b1/c1/d1"), DiffType::LeftOnly(meta(10))),
                DiffEntry::new(
                    repo_path_buf("a1/b2"),
                    DiffType::Changed(meta(20), meta(40))
                ),
                DiffEntry::new(repo_path_buf("a2/b2/c2"), DiffType::RightOnly(meta(30))),
            )
        );
        right
            .insert(repo_path_buf("a1/b1/c1/d1"), meta(10))
            .unwrap();
        left.insert(repo_path_buf("a1/b2"), meta(40)).unwrap();
        left.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();

        assert!(diff(&left, &right).next().is_none());
    }

    #[test]
    fn test_diff_does_not_evaluate_durable_on_node_equality() {
        // Leaving the store empty intentionaly so that we get a panic if anything is read from it.
        let left = Tree::durable(TestStore::new(), Node::from_u8(10));
        let right = Tree::durable(TestStore::new(), Node::from_u8(10));
        assert!(diff(&left, &right).next().is_none());

        let right = Tree::durable(TestStore::new(), Node::from_u8(20));
        assert!(diff(&left, &right).next().unwrap().is_err());
    }

    #[test]
    fn test_diff_one_file_one_directory() {
        let mut left = Tree::ephemeral(TestStore::new());
        left.insert(repo_path_buf("a1/b1"), meta(10)).unwrap();
        left.insert(repo_path_buf("a2"), meta(20)).unwrap();

        let mut right = Tree::ephemeral(TestStore::new());
        right.insert(repo_path_buf("a1"), meta(30)).unwrap();
        right.insert(repo_path_buf("a2/b2"), meta(40)).unwrap();

        assert_eq!(
            diff(&left, &right).collect::<Fallible<Vec<_>>>().unwrap(),
            vec!(
                DiffEntry::new(repo_path_buf("a1"), DiffType::RightOnly(meta(30))),
                DiffEntry::new(repo_path_buf("a1/b1"), DiffType::LeftOnly(meta(10))),
                DiffEntry::new(repo_path_buf("a2"), DiffType::LeftOnly(meta(20))),
                DiffEntry::new(repo_path_buf("a2/b2"), DiffType::RightOnly(meta(40))),
            )
        );
    }

    #[test]
    fn test_diff_left_empty() {
        let mut left = Tree::ephemeral(TestStore::new());

        let mut right = Tree::ephemeral(TestStore::new());
        right
            .insert(repo_path_buf("a1/b1/c1/d1"), meta(10))
            .unwrap();
        right.insert(repo_path_buf("a1/b2"), meta(20)).unwrap();
        right.insert(repo_path_buf("a2/b2/c2"), meta(30)).unwrap();

        assert_eq!(
            diff(&left, &right).collect::<Fallible<Vec<_>>>().unwrap(),
            vec!(
                DiffEntry::new(repo_path_buf("a1/b1/c1/d1"), DiffType::RightOnly(meta(10))),
                DiffEntry::new(repo_path_buf("a1/b2"), DiffType::RightOnly(meta(20))),
                DiffEntry::new(repo_path_buf("a2/b2/c2"), DiffType::RightOnly(meta(30))),
            )
        );

        left.flush().unwrap();
        right.flush().unwrap();

        assert_eq!(
            diff(&left, &right).collect::<Fallible<Vec<_>>>().unwrap(),
            vec!(
                DiffEntry::new(repo_path_buf("a1/b1/c1/d1"), DiffType::RightOnly(meta(10))),
                DiffEntry::new(repo_path_buf("a1/b2"), DiffType::RightOnly(meta(20))),
                DiffEntry::new(repo_path_buf("a2/b2/c2"), DiffType::RightOnly(meta(30))),
            )
        );
    }
}
