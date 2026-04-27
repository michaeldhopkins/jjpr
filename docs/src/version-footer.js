document.addEventListener('DOMContentLoaded', function() {
    var nav = document.querySelector('.nav-wide-wrapper') || document.querySelector('.nav-wrapper');
    if (nav) {
        var footer = document.createElement('div');
        footer.className = 'version-footer';
        footer.textContent = 'jjpr v0.19.0';
        nav.parentNode.insertBefore(footer, nav.nextSibling);
    }
});
