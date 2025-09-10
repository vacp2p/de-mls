export const manifest = {
	appDir: "_app",
	appPath: "_app",
	assets: new Set(["favicon.png"]),
	mimeTypes: {".png":"image/png"},
	_: {
		client: {"start":{"file":"_app/immutable/entry/start.530cd74f.js","imports":["_app/immutable/entry/start.530cd74f.js","_app/immutable/chunks/index.b5cfe40e.js","_app/immutable/chunks/singletons.995fdd8e.js","_app/immutable/chunks/index.0a9737cc.js"],"stylesheets":[],"fonts":[]},"app":{"file":"_app/immutable/entry/app.e73d9bc3.js","imports":["_app/immutable/entry/app.e73d9bc3.js","_app/immutable/chunks/index.b5cfe40e.js"],"stylesheets":[],"fonts":[]}},
		nodes: [
			() => import('./nodes/0.js'),
			() => import('./nodes/1.js'),
			() => import('./nodes/2.js'),
			() => import('./nodes/3.js')
		],
		routes: [
			{
				id: "/",
				pattern: /^\/$/,
				params: [],
				page: { layouts: [0], errors: [1], leaf: 2 },
				endpoint: null
			},
			{
				id: "/chat",
				pattern: /^\/chat\/?$/,
				params: [],
				page: { layouts: [0], errors: [1], leaf: 3 },
				endpoint: null
			}
		],
		matchers: async () => {
			
			return {  };
		}
	}
};
