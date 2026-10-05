export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.hostname === 'haps.iris.to') {
      url.protocol = 'https:';
      url.hostname = 'haps.hashtree.cc';
      url.port = '';
      return Response.redirect(url.toString(), 308);
    }
    return env.ASSETS.fetch(request);
  }
};
