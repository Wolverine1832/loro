use smallvec::SmallVec;

use crate::{
    rle_tree::{
        arena::VecTrait,
        cursor::SafeCursorMut,
        tree_trait::{FindPosResult, InsertResult, Position},
    },
    HasLength, Sliceable,
};
use std::fmt::{Debug, Error, Formatter};

use super::{utils::distribute, *};

impl<'bump, T: Rle, A: RleTreeTrait<T>> LeafNode<'bump, T, A> {
    #[inline]
    pub fn new(bump: &'bump A::Arena, parent: NonNull<InternalNode<'bump, T, A>>) -> Self {
        Self {
            bump,
            parent,
            children: <<A::Arena as Arena>::Vec<'bump, _> as VecTrait<_>>::with_capacity_in(
                A::MAX_CHILDREN_NUM,
                bump,
            ),
            prev: None,
            next: None,
            cache: Default::default(),
            _pin: PhantomPinned,
            _a: PhantomData,
        }
    }

    #[inline]
    fn _split<F>(&mut self, notify: &mut F) -> <A::Arena as Arena>::Boxed<'bump, Node<'bump, T, A>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let mut ans = self
            .bump
            .allocate(Node::Leaf(Self::new(self.bump, self.parent)));
        let ans_inner = ans.as_leaf_mut().unwrap();
        let ans_ptr = ans_inner as _;
        for child in self
            .children
            .drain(self.children.len() - A::MIN_CHILDREN_NUM..self.children.len())
        {
            notify(&child, ans_ptr);
            ans_inner.children.push(child);
        }

        Self::connect(Some(ans_inner), self.next_mut());
        Self::connect(Some(self), Some(ans_inner));
        ans
    }

    #[inline]
    fn connect(a: Option<&mut LeafNode<'bump, T, A>>, b: Option<&mut LeafNode<'bump, T, A>>) {
        match (a, b) {
            (None, None) => {}
            (None, Some(next)) => next.prev = None,
            (Some(prev), None) => prev.next = None,
            (Some(a), Some(b)) => {
                a.next = Some(NonNull::new(b).unwrap());
                b.prev = Some(NonNull::new(a).unwrap());
            }
        }
    }

    #[inline]
    pub fn get_cursor<'tree>(&'tree self, pos: A::Int) -> SafeCursor<'bump, T, A> {
        let result = A::find_pos_leaf(self, pos);
        assert!(result.found);
        SafeCursor::from_leaf(self, result.child_index, result.offset, result.pos, 0)
    }

    #[inline]
    pub fn get_cursor_mut<'b>(&'b mut self, pos: A::Int) -> SafeCursorMut<'bump, T, A> {
        let result = A::find_pos_leaf(self, pos);
        assert!(result.found);
        SafeCursorMut::from_leaf(self, result.child_index, result.offset, result.pos, 0)
    }

    pub fn push_child<F>(
        &mut self,
        value: T,
        notify: &mut F,
    ) -> Result<(), <A::Arena as Arena>::Boxed<'bump, Node<'bump, T, A>>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let self_ptr = self as *mut _;
        if !self.children.is_empty() {
            let last = self.children.last_mut().unwrap();
            if last.is_mergable(&value, &()) {
                last.merge(&value, &());
                notify(last, self_ptr);
                A::update_cache_leaf(self);
                return Ok(());
            }
        }

        if self.children.len() == A::MAX_CHILDREN_NUM {
            let mut ans = self._split(notify);
            let inner = ans.as_leaf_mut().unwrap();
            inner.push_child(value, notify).unwrap();
            A::update_cache_leaf(self);
            A::update_cache_leaf(inner);
            return Err(ans);
        }

        self.children.push(value);
        notify(&self.children[self.children.len() - 1], self_ptr);
        A::update_cache_leaf(self);
        Ok(())
    }

    pub(crate) fn check(&self) {
        assert!(self.children.len() <= A::MAX_CHILDREN_NUM);
        // assert!(self.children.len() >= A::MIN_CHILDREN_NUM);
        assert!(!self.is_deleted());
        A::check_cache_leaf(self);
        if let Some(next) = self.next {
            // SAFETY: this is only for testing, and next must be a valid pointer
            let self_ptr = unsafe { next.as_ref().prev.unwrap().as_ptr() };
            // SAFETY: this is only for testing, and next must be a valid pointer
            assert!(unsafe { !next.as_ref().is_deleted() });
            assert!(std::ptr::eq(self, self_ptr));
        }
        if let Some(prev) = self.prev {
            // SAFETY: this is only for testing, and prev must be a valid pointer
            let self_ptr = unsafe { prev.as_ref().next.unwrap().as_ptr() };
            // SAFETY: this is only for testing, and next must be a valid pointer
            assert!(unsafe { !prev.as_ref().is_deleted() });
            assert!(std::ptr::eq(self, self_ptr));
        }
    }

    fn _delete_start(&mut self, from: A::Int) -> (usize, Option<usize>) {
        let result = A::find_pos_leaf(self, from);
        match result.pos {
            Position::Start | Position::Before => (result.child_index, None),
            Position::Middle | Position::End | Position::After => {
                (result.child_index + 1, Some(result.offset))
            }
        }
    }

    fn _delete_end(&mut self, to: A::Int) -> (usize, Option<usize>) {
        let result = A::find_pos_leaf(self, to);
        match result.pos {
            Position::After | Position::End => (result.child_index + 1, None),
            Position::Start | Position::Middle | Position::Before => {
                (result.child_index, Some(result.offset))
            }
        }
    }

    pub fn is_deleted(&self) -> bool {
        // SAFETY: In HeapMode this function should always returns true.
        // In BumpMode we may keep the pointer to the leaf even if it's deleted
        unsafe {
            let mut node = self.parent.as_ref();
            if !node
                .children
                .iter()
                .any(|x| std::ptr::eq(x.node.as_leaf().unwrap(), self))
            {
                return true;
            }

            while let Some(parent) = node.parent {
                let parent = parent.as_ref();
                if !parent
                    .children()
                    .iter()
                    .any(|x| std::ptr::eq(x.node.as_internal().unwrap(), node))
                {
                    return true;
                }

                node = parent;
            }
        }

        false
    }

    #[allow(clippy::type_complexity)]
    pub fn insert<F>(
        &mut self,
        raw_index: A::Int,
        value: T,
        notify: &mut F,
    ) -> Result<
        A::CacheInParent,
        (
            A::CacheInParent,
            <A::Arena as Arena>::Boxed<'bump, Node<'bump, T, A>>,
        ),
    >
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let result = {
            if self.children.is_empty() {
                notify(&value, self);
                self.children.push(value);
                Ok(())
            } else {
                let FindPosResult {
                    child_index,
                    offset,
                    pos,
                    ..
                } = A::find_pos_leaf(self, raw_index);
                self._insert_at_pos(pos, child_index, offset, value, notify, false)
            }
        };
        self.with_cache_updated(result)
    }

    pub(crate) fn insert_at_pos<F>(
        &mut self,
        pos: Position,
        child_index: usize,
        offset: usize,
        value: T,
        notify: &mut F,
        value_from_same_parent: bool,
    ) -> InsertResult<'bump, T, A>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let result = {
            if self.children.is_empty() {
                if !value_from_same_parent {
                    notify(&value, self);
                }
                self.children.push(value);
                Ok(())
            } else {
                self._insert_at_pos(
                    pos,
                    child_index,
                    offset,
                    value,
                    notify,
                    value_from_same_parent,
                )
            }
        };
        self.with_cache_updated(result)
    }

    /// update the content at given selection
    pub(crate) fn update_at_pos<F, U>(
        &mut self,
        pos: Position,
        child_index: usize,
        offset: usize,
        len: usize,
        update_fn: U,
        notify: &mut F,
    ) -> InsertResult<'bump, T, A>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
        U: FnOnce(&mut T),
    {
        if len == 0 {
            return Ok(Default::default());
        }

        if offset == 0 && self.children[child_index].atom_len() == len {
            update_fn(&mut self.children[child_index]);
            return Ok(Default::default());
        }

        let left = if offset == 0 {
            None
        } else {
            Some(self.children[child_index].slice(0, offset))
        };

        let right = if self.children[child_index].atom_len() == offset + len {
            None
        } else {
            Some(
                self.children[child_index]
                    .slice(offset + len, self.children[child_index].atom_len()),
            )
        };

        let mut target = self.children[child_index].slice(offset, offset + len);
        let old_cache = A::value_to_update(&target);
        update_fn(&mut target);
        let update = A::value_to_update(&target) - old_cache;

        if let Some(left) = left {
            self.children[child_index] = left;
            let left = &mut self.children[child_index];
            if left.is_mergable(&target, &()) {
                left.merge(&target, &());
                if let Some(right) = right {
                    if left.is_mergable(&right, &()) {
                        left.merge(&right, &());
                        Ok(update)
                    } else {
                        self.insert_at_pos(Position::Start, child_index + 1, 0, right, notify, true)
                            .map(|x| (x + update))
                            .map_err(|(x, new)| (x + update, new))
                    }
                } else {
                    Ok(update)
                }
            } else if let Some(right) = right {
                if target.is_mergable(&right, &()) {
                    target.merge(&right, &());
                    self.insert_at_pos(Position::Start, child_index + 1, 0, target, notify, true)
                        .map(|x| (x + update))
                        .map_err(|(x, new)| (x + update, new))
                } else {
                    let result = self.insert_at_pos(
                        Position::Start,
                        child_index + 1,
                        0,
                        target,
                        notify,
                        true,
                    );
                    if let Err((x, mut new)) = result {
                        if self.children.len() >= child_index + 2 {
                            // insert one element should not cause Err
                            self.insert_at_pos(
                                Position::Start,
                                child_index + 2,
                                0,
                                right,
                                notify,
                                true,
                            )
                            .map(|_x| ())
                            .map_err(|(_, new)| new)
                            .unwrap();
                            Err((x + update, new))
                        } else {
                            let new_insert_index = child_index + 2 - self.children.len();
                            // insert one element should not cause Err
                            new.as_leaf_mut()
                                .unwrap()
                                .insert_at_pos(
                                    Position::Start,
                                    new_insert_index,
                                    0,
                                    right,
                                    notify,
                                    true,
                                )
                                .unwrap();
                            Err((x + update, new))
                        }
                    } else {
                        self.insert_at_pos(Position::Start, child_index + 2, 0, right, notify, true)
                            .map(|x| (x + update))
                            .map_err(|(x, new)| (x + update, new))
                    }
                }
            } else {
                self.insert_at_pos(pos, child_index + 1, offset, target, notify, true)
                    .map(|x| (x + update))
                    .map_err(|(x, new)| (x + update, new))
            }
        } else {
            self.children[child_index] = target;
            if let Some(right) = right {
                self.insert_at_pos(Position::Start, child_index + 1, 0, right, notify, true)
                    .map(|x| (x + update))
                    .map_err(|(x, new)| (x + update, new))
            } else {
                Ok(update)
            }
        }
    }

    /// this is a effect-less operation, it will not modify the data, it returns the needed change at the given index instead
    pub(crate) fn pure_update<U>(
        &self,
        child_index: usize,
        offset: usize,
        len: usize,
        update_fn: &mut U,
    ) -> Option<SmallVec<[T; 4]>>
    where
        U: FnMut(&mut T),
    {
        let mut ans = smallvec::smallvec![];
        if len == 0 {
            return None;
        }

        let child = &self.children[child_index];
        if offset == 0 && child.atom_len() == len {
            let mut element = child.clone();
            update_fn(&mut element);
            ans.push(element);
            return Some(ans);
        }

        if offset != 0 {
            ans.push(child.slice(0, offset));
        }
        let mut target = child.slice(offset, offset + len);
        update_fn(&mut target);
        if !ans.is_empty() {
            if ans[0].is_mergable(&target, &()) {
                ans[0].merge(&target, &());
            } else {
                ans.push(target);
            }
        } else {
            ans.push(target);
        }

        if offset + len < child.atom_len() {
            let right = child.slice(offset + len, child.atom_len());
            let mut merged = false;
            if let Some(last) = ans.last_mut() {
                if last.is_mergable(&right, &()) {
                    merged = true;
                    last.merge(&right, &());
                }
            }

            if !merged {
                ans.push(right);
            }
        }

        Some(ans)
    }

    #[inline(always)]
    pub(crate) fn pure_updates_at_same_index<'a, U, Arg, Lens, Offsets, Args>(
        &self,
        child_index: usize,
        offsets: Offsets,
        mut lens: Lens,
        mut args: Args,
        update_fn: &mut U,
    ) -> SmallVec<[T; 4]>
    where
        U: FnMut(&mut T, &Arg),
        Offsets: Iterator<Item = usize>,
        Lens: Iterator<Item = usize>,
        Args: Iterator<Item = &'a Arg>,
        Arg: 'a,
    {
        let mut ans: SmallVec<[T; 4]> = SmallVec::new();
        let ans_len = self.children[child_index].atom_len();
        for (_, offset) in offsets.enumerate() {
            if ans.is_empty() {
                ans.push(self.children[child_index].clone());
            }
            let len = lens.next().unwrap();
            let arg = args.next().unwrap();
            // TODO: can be optimized if needed
            let mut target_spans = ans.slice(offset, offset + len);
            for span in target_spans.iter_mut() {
                update_fn(span, arg);
            }

            let mut end = ans.slice(offset + len, ans_len);
            ans = ans.slice(0, offset);
            ans.append(&mut target_spans);
            ans.append(&mut end);
        }

        debug_assert_eq!(ans_len, ans.iter().map(|x| x.atom_len()).sum());
        ans
    }

    // TODO: refactor
    #[allow(clippy::type_complexity)]
    pub(crate) fn apply_updates<F>(
        &mut self,
        mut updates: Vec<(usize, SmallVec<[T; 4]>)>,
        notify: &mut F,
    ) -> Result<A::CacheInParent, (A::CacheInParent, Vec<ArenaBoxedNode<'bump, T, A>>)>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        if updates.is_empty() {
            return Ok(self.cache.into());
        }

        updates.retain(|x| !x.1.is_empty());
        updates.sort_by_key(|x| x.0);
        let new_len: usize =
            updates.iter().map(|x| x.1.len() - 1).sum::<usize>() + self.children.len();
        if new_len <= A::MAX_CHILDREN_NUM {
            let mut offset = 0;
            for (index, replace) in updates {
                let replace_len = replace.len();
                if replace_len == 1 {
                    self.children[index + offset] = replace.into_iter().next().unwrap();
                } else {
                    self.children
                        .splice(index + offset..index + offset + 1, replace);
                    offset += replace_len - 1;
                }
            }

            // TODO: try merging here?
            Ok(A::update_cache_leaf(self))
        } else {
            let mut new_children: SmallVec<[_; 64]> = SmallVec::new();
            let mut last_end = 0;
            // append element to the new_children list
            for (index, replace) in updates {
                for child in self.children.drain(0..index + 1 - last_end) {
                    new_children.push(child);
                }

                new_children.pop();

                for element in replace {
                    let mut merged = false;
                    if let Some(last) = new_children.last_mut() {
                        if last.is_mergable(&element, &()) {
                            last.merge(&element, &());
                            merged = true;
                        }
                    }
                    if !merged {
                        new_children.push(element);
                    }
                }

                last_end = index + 1;
            }

            for child in self.children.drain(..) {
                new_children.push(child);
            }

            let children_nums =
                distribute(new_children.len(), A::MIN_CHILDREN_NUM, A::MAX_CHILDREN_NUM);
            let mut index = 0;
            for child in new_children.drain(..children_nums[index]) {
                self.children.push(child);
            }

            index += 1;
            let ans = A::update_cache_leaf(self);
            let mut leaf_vec = Vec::new();
            while !new_children.is_empty() {
                let mut new_leaf_node = self
                    .bump
                    .allocate(Node::Leaf(LeafNode::new(self.bump, self.parent)));
                let new_leaf = new_leaf_node.as_leaf_mut().unwrap();
                for child in new_children.drain(..children_nums[index]) {
                    notify(&child, new_leaf);
                    new_leaf.children.push(child);
                }

                index += 1;
                A::update_cache_leaf(new_leaf);
                leaf_vec.push(new_leaf_node);
            }

            let next = self.next;
            let mut last = self;
            for leaf in leaf_vec.iter_mut() {
                Self::connect(Some(last), Some(leaf.as_leaf_mut().unwrap()));
                last = leaf.as_leaf_mut().unwrap();
            }

            // SAFETY: there will not be shared mutable references
            Self::connect(Some(last), unsafe { next.map(|mut x| x.as_mut()) });
            Err((ans, leaf_vec))
        }
    }

    #[allow(clippy::type_complexity)]
    fn with_cache_updated(
        &mut self,
        result: Result<(), <A::Arena as Arena>::Boxed<'bump, Node<'bump, T, A>>>,
    ) -> Result<
        A::CacheInParent,
        (
            A::CacheInParent,
            <A::Arena as Arena>::Boxed<'bump, Node<'bump, T, A>>,
        ),
    > {
        match result {
            Ok(_) => Ok(A::update_cache_leaf(self)),
            Err(mut new) => {
                let update = A::update_cache_leaf(self);
                A::update_cache_leaf(new.as_leaf_mut().unwrap());
                Err((update, new))
            }
        }
    }

    fn _insert_at_pos<F>(
        &mut self,
        mut pos: Position,
        mut child_index: usize,
        mut offset: usize,
        value: T,
        notify: &mut F,
        value_from_same_parent: bool,
    ) -> Result<(), <A::Arena as Arena>::Boxed<'bump, Node<'bump, T, A>>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        let self_ptr = self as *mut _;
        let prev = {
            if (pos == Position::Start || pos == Position::Before) && child_index > 0 {
                Some(&mut self.children[child_index - 1])
            } else if pos == Position::After || pos == Position::End {
                child_index += 1;
                offset = 0;
                pos = Position::Start;
                Some(&mut self.children[child_index - 1])
            } else {
                None
            }
        };
        if let Some(prev) = prev {
            // clean cut, should no split
            if prev.is_mergable(&value, &()) {
                prev.merge(&value, &());
                if !value_from_same_parent {
                    notify(prev, self_ptr);
                }
                return Ok(());
            }
        }
        let clean_cut = pos != Position::Middle;
        if clean_cut {
            return self._insert_with_split(child_index, value, notify, false);
        }
        // need to split child
        let a = self.children[child_index].slice(0, offset);
        let b = self.children[child_index].slice(offset, self.children[child_index].atom_len());
        self.children[child_index] = a;
        if self.children.len() >= A::MAX_CHILDREN_NUM - 1 {
            let mut next_node = self._split(notify);
            let next_leaf = next_node.as_leaf_mut().unwrap();
            if child_index < self.children.len() {
                if !value_from_same_parent {
                    notify(&value, self_ptr);
                }
                self.children.insert(child_index + 1, value);
                self.children.insert(child_index + 2, b);

                let last_child = self.children.pop().unwrap();
                notify(&last_child, next_leaf);
                next_leaf.children.insert(0, last_child);
            } else {
                notify(&value, next_leaf);
                next_leaf
                    .children
                    .insert(child_index - self.children.len() + 1, value);
                notify(&b, next_leaf);
                next_leaf
                    .children
                    .insert(child_index - self.children.len() + 2, b);
            }

            return Err(next_node);
        }
        if !value_from_same_parent {
            notify(&value, self);
        }
        self.children.insert(child_index + 1, b);
        self.children.insert(child_index + 1, value);
        Ok(())
    }

    #[inline(always)]
    pub fn next(&self) -> Option<&Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.next.map(|p| p.as_ref()) }
    }

    #[inline(always)]
    pub fn next_mut(&mut self) -> Option<&mut Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.next.map(|mut p| p.as_mut()) }
    }

    #[inline(always)]
    pub fn prev(&self) -> Option<&Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.prev.map(|p| p.as_ref()) }
    }

    #[inline(always)]
    pub fn prev_mut(&mut self) -> Option<&mut Self> {
        // SAFETY: internal variant ensure prev and next are valid reference
        unsafe { self.prev.map(|mut p| p.as_mut()) }
    }

    #[inline(always)]
    pub fn children(&self) -> &<<A as RleTreeTrait<T>>::Arena as Arena>::Vec<'bump, T> {
        &self.children
    }

    #[inline(always)]
    pub fn parent(&self) -> &NonNull<InternalNode<'bump, T, A>> {
        &self.parent
    }
}

impl<'a, T: Rle, A: RleTreeTrait<T>> LeafNode<'a, T, A> {
    /// Delete may cause the children num increase, because splitting may happen
    ///
    pub(crate) fn delete<F>(
        &mut self,
        start: Option<A::Int>,
        end: Option<A::Int>,
        notify: &mut F,
    ) -> InsertResult<'a, T, A>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        if self.children.is_empty() {
            return Ok(Default::default());
        }

        let (del_start, del_relative_from) = start.map_or((0, None), |x| self._delete_start(x));
        let (del_end, del_relative_to) =
            end.map_or((self.children.len(), None), |x| self._delete_end(x));
        let mut handled = false;
        let mut result = Ok(());
        if let (Some(del_relative_from), Some(del_relative_to)) =
            (del_relative_from, del_relative_to)
        {
            if del_start - 1 == del_end {
                let end = &mut self.children[del_end];
                let (left, right) = (
                    end.slice(0, del_relative_from),
                    end.slice(del_relative_to, end.atom_len()),
                );

                *end = left;
                result = self._insert_with_split(del_end + 1, right, notify, true);
                handled = true;
            }
        }

        if !handled {
            if let Some(del_relative_from) = del_relative_from {
                self.children[del_start - 1] =
                    self.children[del_start - 1].slice(0, del_relative_from);
            }
            if let Some(del_relative_to) = del_relative_to {
                let end = &mut self.children[del_end];
                *end = end.slice(del_relative_to, end.atom_len());
            }
        }

        if del_start < del_end {
            for _ in self.children.drain(del_start..del_end) {}
        }

        let diff = A::update_cache_leaf(self);
        if let Err(new) = &mut result {
            A::update_cache_leaf(new.as_leaf_mut().unwrap());
        }

        match result {
            Ok(_) => Ok(diff),
            Err(x) => Err((diff, x)),
        }
    }

    fn _insert_with_split<F>(
        &mut self,
        index: usize,
        value: T,
        notify: &mut F,
        value_from_same_parent: bool,
    ) -> Result<(), <A::Arena as Arena>::Boxed<'a, Node<'a, T, A>>>
    where
        F: FnMut(&T, *mut LeafNode<'_, T, A>),
    {
        if self.children.len() == A::MAX_CHILDREN_NUM {
            let mut ans = self._split(notify);
            if index <= self.children.len() {
                if !value_from_same_parent {
                    notify(&value, self);
                }
                self.children.insert(index, value);
            } else {
                let leaf = ans.as_leaf_mut().unwrap();
                notify(&value, leaf);
                leaf.children.insert(index - self.children.len(), value);
            }

            Err(ans)
        } else {
            if !value_from_same_parent {
                notify(&value, self);
            }
            self.children.insert(index, value);
            Ok(())
        }
    }

    pub fn get_index_in_parent(&self) -> Option<usize> {
        let parent = self.parent;
        // SAFETY: we know parent must be valid
        let parent = unsafe { parent.as_ref() };
        parent
            .children
            .iter()
            .position(|child| std::ptr::eq(child.node.as_leaf().unwrap(), self))
    }

    #[inline(always)]
    pub(crate) fn update_cache(&mut self) {
        A::update_cache_leaf(self);
    }
}

impl<'a, T: Rle, A: RleTreeTrait<T>> Debug for LeafNode<'a, T, A> {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        let mut debug_struct = f.debug_struct("LeafNode");
        debug_struct.field("children", &self.children);
        debug_struct.field("cache", &self.cache);
        debug_struct.field("children_num", &self.children.len());
        debug_struct.finish()
    }
}

fn slice<T: HasLength + Sliceable>(
    vec: &[T],
    beginning: usize,
    from: usize,
    to: usize,
) -> SmallVec<[T; 2]> {
    let mut index = beginning;
    let mut ans = smallvec::smallvec![];
    for item in vec.iter() {
        if index < to && from < index + item.atom_len() {
            let start = if index < from { from - index } else { 0 };
            let len = (item.atom_len() - start).min(to - index);
            ans.push(item.slice(start, start + len));
        }

        index += item.atom_len();
    }

    ans
}
