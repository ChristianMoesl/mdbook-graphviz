use futures_util::future;
use futures_util::future::{BoxFuture, FutureExt};
use mdbook::book::{Book, Chapter};
use mdbook::errors::Error;
use mdbook::errors::Result;
use mdbook::preprocess::{Preprocessor, PreprocessorContext};
use mdbook::BookItem;
use pulldown_cmark::{Event, LinkType, Parser, Tag};
use pulldown_cmark_to_cmark::fmt::cmark;
use std::mem;
use std::path::PathBuf;
use tokio::runtime::Runtime;

use crate::renderer::{CommandLineGraphviz, GraphvizRenderer};

pub static PREPROCESSOR_NAME: &str = "mdbook-graphviz";
pub static INFO_STRING_PREFIX: &str = "dot process";

pub struct Graphviz {
    renderer: Box<dyn GraphvizRenderer + Sync>,
}

impl Preprocessor for Graphviz {
    fn name(&self) -> &str {
        PREPROCESSOR_NAME
    }

    fn run(&self, ctx: &PreprocessorContext, original_book: Book) -> Result<Book> {
        let runtime = Runtime::new()?;

        let src_dir = ctx.root.clone().join(&ctx.config.book.src);

        let mut processed_book = original_book.clone();

        let section_futures = mem::replace(&mut processed_book.sections, vec![])
            .into_iter()
            .map(|section| self.process_section(section, &src_dir));

        let sections = runtime
            .block_on(future::join_all(section_futures))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        processed_book.sections = sections;

        Ok(processed_book)
    }

    fn supports_renderer(&self, _renderer: &str) -> bool {
        // since we're just outputting markdown images, this should support any renderer
        true
    }
}

impl Graphviz {
    pub fn command_line_renderer() -> Graphviz {
        let renderer = CommandLineGraphviz;

        Graphviz {
            renderer: Box::new(renderer),
        }
    }

    fn process_section<'a>(
        &'a self,
        section: BookItem,
        src_dir: &'a PathBuf,
    ) -> BoxFuture<'a, Result<BookItem>> {
        if let BookItem::Chapter(original_chapter) = section {
            let mut full_path = src_dir.join(&original_chapter.path);

            // remove the chapter filename
            full_path.pop();

            async move {
                // process the current chapter we're on as a leaf
                match self
                    .process_leaf_chapter(original_chapter, &full_path)
                    .await
                {
                    Ok(mut chapter) => {
                        // if our chapter processed, descend into any sub chapters
                        self.process_sub_items(
                            mem::replace(&mut chapter.sub_items, vec![]),
                            src_dir,
                        )
                        .await
                        .map(|sub_items| {
                            chapter.sub_items = sub_items;

                            chapter
                        })
                    }
                    e => e,
                }
                .map(BookItem::Chapter)
            }
            .boxed()
        } else {
            future::ready(Ok(section)).boxed()
        }
    }

    /// Process all the sub items for a chapter
    async fn process_sub_items(
        &self,
        sub_items: Vec<BookItem>,
        src_dir: &PathBuf,
    ) -> Result<Vec<BookItem>> {
        let sub_futures = sub_items
            .into_iter()
            .map(|section| self.process_section(section, &src_dir));

        future::join_all(sub_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
    }

    /// Process this chapter, ignoring any potential sub_items
    async fn process_leaf_chapter(
        &self,
        mut chapter: Chapter,
        chapter_path: &PathBuf,
    ) -> Result<Chapter> {
        let mut buf = String::with_capacity(chapter.content.len());
        let mut graphviz_block_builder: Option<GraphvizBlockBuilder> = None;
        let mut image_index = 0;

        let event_futures: Vec<_> = Parser::new(&chapter.content)
            .map(|e| -> BoxFuture<Result<Vec<Event>>> {
                if let Some(ref mut builder) = graphviz_block_builder {
                    match e {
                        Event::Text(ref text) => {
                            builder.append_code(&**text);

                            future::ready(Ok(vec![])).boxed()
                        }
                        Event::End(Tag::CodeBlock(ref info_string)) => {
                            assert_eq!(
                                Some(0),
                                (&**info_string).find(INFO_STRING_PREFIX),
                                "We must close our graphviz block"
                            );

                            // finish our digraph
                            let block = builder.build(image_index);
                            image_index += 1;
                            graphviz_block_builder = None;

                            let tag_events = block.tag_events();

                            block
                                .render_graphviz(&*self.renderer)
                                .map(|r| r.map(|_| tag_events))
                                .boxed()
                        }
                        _ => future::ready(Ok(vec![e])).boxed(),
                    }
                } else {
                    match e {
                        Event::Start(Tag::CodeBlock(ref info_string))
                            if (&**info_string).find(INFO_STRING_PREFIX) == Some(0) =>
                        {
                            graphviz_block_builder = Some(GraphvizBlockBuilder::new(
                                &**info_string,
                                &chapter.name.clone(),
                                chapter_path.clone(),
                            ));

                            future::ready(Ok(vec![])).boxed()
                        }
                        _ => future::ready(Ok(vec![e])).boxed(),
                    }
                }
            })
            .collect();

        // join all our futures back up and handle the results
        let events = future::join_all(event_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|e| e);

        cmark(events, &mut buf, None)
            .map_err(|err| Error::from(format!("Markdown serialization failed: {}", err)))?;

        chapter.content = buf;

        Ok(chapter)
    }
}

struct GraphvizBlockBuilder {
    chapter_name: String,
    graph_name: String,
    code: String,
    path: PathBuf,
}

impl GraphvizBlockBuilder {
    fn new<S: Into<String>>(
        info_string: S,
        chapter_name: S,
        path: PathBuf,
    ) -> GraphvizBlockBuilder {
        let info_string: String = info_string.into();

        let chapter_name = chapter_name.into();

        let mut graph_name = "";
        // check if we can have a name at the end of our info string
        if Some(' ') == info_string.chars().nth(INFO_STRING_PREFIX.len()) {
            graph_name = &info_string[INFO_STRING_PREFIX.len() + 1..].trim();
        }

        GraphvizBlockBuilder {
            chapter_name: chapter_name.trim().into(),
            graph_name: graph_name.into(),
            code: String::new(),
            path,
        }
    }

    fn append_code<S: Into<String>>(&mut self, code: S) {
        self.code.push_str(&code.into());
    }

    fn build(&self, index: usize) -> GraphvizBlock {
        let cleaned_code = self.code.trim();

        let image_name = if !self.graph_name.is_empty() {
            format!(
                "{}_{}_{}.generated",
                normalize_id(&self.chapter_name),
                normalize_id(&self.graph_name),
                index
            )
        } else {
            format!("{}_{}.generated", normalize_id(&self.chapter_name), index)
        };

        GraphvizBlock::new(
            self.graph_name.clone(),
            image_name,
            cleaned_code.into(),
            self.path.clone(),
        )
    }
}

struct GraphvizBlock {
    graph_name: String,
    image_name: String,
    code: String,
    chapter_path: PathBuf,
}

impl GraphvizBlock {
    fn new<S: Into<String>>(graph_name: S, image_name: S, code: S, path: PathBuf) -> GraphvizBlock {
        let image_name = image_name.into();

        GraphvizBlock {
            graph_name: graph_name.into(),
            image_name,
            code: code.into(),
            chapter_path: path,
        }
    }

    fn tag_events<'a, 'b>(&'a self) -> Vec<Event<'b>> {
        vec![
            Event::Start(self.image_tag()),
            Event::End(self.image_tag()),
            Event::Text("\n\n".into()),
        ]
    }

    async fn render_graphviz(self, renderer: &(dyn GraphvizRenderer + Sync)) -> Result<()> {
        let output_path = self.chapter_path.join(self.file_name());

        renderer.render_graphviz(&self.code, &output_path).await
    }

    fn image_tag<'a, 'b>(&'a self) -> Tag<'b> {
        Tag::Image(
            LinkType::Inline,
            self.file_name().into(),
            self.graph_name.clone().into(),
        )
    }

    fn file_name(&self) -> String {
        format!("{}.svg", self.image_name)
    }
}

fn normalize_id(content: &str) -> String {
    content
        .chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() || ch == '_' || ch == '-' {
                Some('_')
            } else {
                None
            }
        })
        .collect::<String>()
}

#[cfg(test)]
mod test {
    use super::*;

    static CHAPTER_NAME: &str = "Test Chapter";
    static NORMALIZED_CHAPTER_NAME: &str = "test_chapter";

    struct NoopRenderer;

    impl GraphvizRenderer for NoopRenderer {
        fn render_graphviz<'a>(
            &self,
            _code: &'a String,
            _output_path: &'a PathBuf,
        ) -> BoxFuture<'a, Result<()>> {
            async { Ok(()) }.boxed()
        }
    }

    #[test]
    fn only_preprocess_flagged_blocks() {
        let expected = r#"# Chapter

````dot
digraph Test {
    a -> b
}
````"#;

        let chapter = new_chapter(expected.into());

        assert_eq!(expected, process_chapter(chapter).unwrap().content);
    }

    #[test]
    fn no_name() {
        let chapter = new_chapter(
            r#"# Chapter
```dot process
digraph Test {
    a -> b
}
```
"#
            .into(),
        );

        let expected = format!(
            r#"# Chapter

![]({}_0.generated.svg)

"#,
            NORMALIZED_CHAPTER_NAME
        );

        assert_eq!(expected, process_chapter(chapter).unwrap().content);
    }

    #[test]
    fn named_blocks() {
        let chapter = new_chapter(
            r#"# Chapter
```dot process Graph Name
digraph Test {
    a -> b
}
```
"#
            .into(),
        );

        let expected = format!(
            r#"# Chapter

![]({}_graph_name_0.generated.svg "Graph Name")

"#,
            NORMALIZED_CHAPTER_NAME
        );

        assert_eq!(expected, process_chapter(chapter).unwrap().content);
    }

    #[test]
    fn multiple_blocks() {
        let chapter = new_chapter(
            r#"# Chapter
```dot process Graph Name
digraph Test {
    a -> b
}
```

```dot process Graph Name
digraph Test {
    a -> b
}
```

```dot process Graph Name
digraph Test {
    a -> b
}
```
"#
            .into(),
        );

        let expected = format!(
            r#"# Chapter

![]({}_graph_name_0.generated.svg "Graph Name")

![]({}_graph_name_1.generated.svg "Graph Name")

![]({}_graph_name_2.generated.svg "Graph Name")

"#,
            NORMALIZED_CHAPTER_NAME, NORMALIZED_CHAPTER_NAME, NORMALIZED_CHAPTER_NAME
        );

        assert_eq!(expected, process_chapter(chapter).unwrap().content);
    }

    fn process_chapter(chapter: Chapter) -> Result<Chapter> {
        let runtime = Runtime::new()?;

        let graphviz = Graphviz {
            renderer: Box::new(NoopRenderer),
        };

        runtime.block_on(graphviz.process_leaf_chapter(chapter, &PathBuf::from("./")))
    }

    fn new_chapter(content: String) -> Chapter {
        Chapter::new(CHAPTER_NAME, content.into(), PathBuf::from("./"), vec![])
    }
}
