//! A bare widget whose rendering is a closure that appends GSK render nodes.
//! Everything we draw (overlay, preview, canvas) goes through the GPU scene
//! graph this way; nothing is rasterised on the CPU.

use gtk::{glib, prelude::*, subclass::prelude::*};
use std::cell::RefCell;

type DrawFn = Box<dyn Fn(&DrawArea, &gtk::Snapshot, f32, f32)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct DrawArea {
        pub func: RefCell<Option<DrawFn>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DrawArea {
        const NAME: &'static str = "HolyShotDrawArea";
        type Type = super::DrawArea;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for DrawArea {
        fn dispose(&self) {
            // The closure usually captures state that owns this widget;
            // dropping it here breaks the reference cycle.
            self.func.borrow_mut().take();
        }
    }

    impl WidgetImpl for DrawArea {
        fn snapshot(&self, sn: &gtk::Snapshot) {
            crate::util::trace_once("first snapshot");
            let obj = self.obj();
            if let Some(f) = self.func.borrow().as_ref() {
                f(&obj, sn, obj.width() as f32, obj.height() as f32);
            }
            crate::util::trace_once("first snapshot done");
        }
    }
}

glib::wrapper! {
    pub struct DrawArea(ObjectSubclass<imp::DrawArea>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl DrawArea {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_draw(&self, f: impl Fn(&DrawArea, &gtk::Snapshot, f32, f32) + 'static) {
        *self.imp().func.borrow_mut() = Some(Box::new(f));
        self.queue_draw();
    }
}
